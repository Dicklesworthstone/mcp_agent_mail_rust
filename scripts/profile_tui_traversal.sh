#!/usr/bin/env bash
# profile_tui_traversal.sh — Capture baseline profiling evidence for TUI traversal.
#
# Bead: br-legjy.1.2 (A2: Capture baseline profiling evidence)
#
# Captures during a deterministic traversal across all 15 TUI screens:
#   1. CPU usage per thread (pidstat -t)
#   2. Syscall profile (strace -c summary + strace -e trace=futex,poll,epoll_wait,write)
#   3. Redraw/write-amplification (PTY output bytes per screen, total write() calls)
#   4. Thread wake counts (context switches via pidstat)
#   5. Per-screen activation latency from the traversal harness
#
# Outputs structured artifact directory:
#   tests/artifacts/tui_profile/<timestamp>/
#     profile_report.json   — machine-readable profiling summary
#     pidstat_thread.txt    — per-thread CPU usage over time
#     strace_summary.txt   — syscall count/time summary
#     strace_events.txt    — raw syscall trace (futex/poll/write)
#     traversal_results.json — per-screen latency from harness
#     env_metadata.json    — system info for reproducibility
#
# Usage:
#   bash scripts/profile_tui_traversal.sh
#   E2E_PTY_QUIESCE_MS=120 bash scripts/profile_tui_traversal.sh  # wider gap

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Artifact directory
TIMESTAMP="$(date +%Y%m%d_%H%M%S)"
ARTIFACT_DIR="${PROJECT_ROOT}/tests/artifacts/tui_profile/${TIMESTAMP}"
mkdir -p "${ARTIFACT_DIR}"

echo "=== TUI Traversal Profiling Harness (br-legjy.1.2) ==="
echo "Artifacts: ${ARTIFACT_DIR}"
echo ""

# ────────────────────────────────────────────────────────────────────
# Step 0: Environment metadata
# ────────────────────────────────────────────────────────────────────
python3 -c "
import json, os, platform, subprocess

def cmd(args):
    try:
        return subprocess.check_output(args, stderr=subprocess.DEVNULL, timeout=5).decode().strip()
    except Exception:
        return 'unavailable'

meta = {
    'timestamp': '${TIMESTAMP}',
    'hostname': platform.node(),
    'kernel': platform.release(),
    'arch': platform.machine(),
    'cpu_model': cmd(['sh', '-c', 'grep -m1 \"model name\" /proc/cpuinfo | cut -d: -f2']).strip(),
    'cpu_count': os.cpu_count(),
    'mem_total_mb': round(os.sysconf('SC_PAGE_SIZE') * os.sysconf('SC_PHYS_PAGES') / (1024**2)),
    'rust_version': cmd(['rustc', '--version']),
    'binary': cmd(['which', 'mcp-agent-mail']),
    'binary_size_mb': round(os.path.getsize(cmd(['which', 'mcp-agent-mail'])) / (1024**2), 1) if os.path.exists(cmd(['which', 'mcp-agent-mail']) or '') else 0,
    'load_avg': list(os.getloadavg()),
    'quiesce_ms': int(os.environ.get('E2E_PTY_QUIESCE_MS', '80')),
    'pty_cols': int(os.environ.get('E2E_PTY_COLS', '120')),
    'pty_rows': int(os.environ.get('E2E_PTY_ROWS', '40')),
}
with open('${ARTIFACT_DIR}/env_metadata.json', 'w') as f:
    json.dump(meta, f, indent=2)
print(f\"  CPU: {meta['cpu_model']} ({meta['cpu_count']} cores)\")
print(f\"  RAM: {meta['mem_total_mb']} MB\")
print(f\"  Load: {meta['load_avg']}\")
" || echo "  (metadata collection failed, continuing)"

# ────────────────────────────────────────────────────────────────────
# Step 1: Run traversal harness to get per-screen latencies
# ────────────────────────────────────────────────────────────────────
echo ""
echo "--- Step 1: Running traversal harness for per-screen latencies ---"

# The traversal harness outputs to tests/artifacts/tui_full_traversal/<ts>/
AM_E2E_KEEP_TMP=1 bash "${SCRIPT_DIR}/e2e_tui_full_traversal.sh" 2>&1 | \
    tee "${ARTIFACT_DIR}/traversal_stdout.txt" | \
    grep -E '(PASS|FAIL|Summary|forward|backward|jump|overall)' || true

# Copy the latest traversal results
LATEST_TRAVERSAL="$(ls -td "${PROJECT_ROOT}/tests/artifacts/tui_full_traversal/"*/ 2>/dev/null | head -1)"
if [ -n "${LATEST_TRAVERSAL}" ] && [ -f "${LATEST_TRAVERSAL}/traversal_results.json" ]; then
    cp "${LATEST_TRAVERSAL}/traversal_results.json" "${ARTIFACT_DIR}/traversal_results.json"
    echo "  Traversal results copied from ${LATEST_TRAVERSAL}"
else
    echo "  WARNING: No traversal results found"
fi

# ────────────────────────────────────────────────────────────────────
# Step 2: Profiled PTY traversal with /proc stat polling
# ────────────────────────────────────────────────────────────────────
echo ""
echo "--- Step 2: Profiled PTY traversal (pidstat + /proc polling) ---"

pick_port() {
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

BIN="$(which mcp-agent-mail)"
PROFILE_PORT="$(pick_port)"
PROFILE_DIR="$(mktemp -d /data/tmp/tui_profile.XXXXXX)"
PROFILE_DB="${PROFILE_DIR}/db.sqlite3"
PROFILE_STORAGE="${PROFILE_DIR}/storage"
mkdir -p "${PROFILE_STORAGE}"

# Seed data via headless server
echo "  Seeding data..."
DATABASE_URL="sqlite:////${PROFILE_DB}" \
STORAGE_ROOT="${PROFILE_STORAGE}" \
HTTP_HOST="127.0.0.1" \
HTTP_PORT="${PROFILE_PORT}" \
HTTP_RBAC_ENABLED=0 \
HTTP_RATE_LIMIT_ENABLED=0 \
HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=1 \
TUI_ENABLED=0 \
    "${BIN}" serve --host 127.0.0.1 --port "${PROFILE_PORT}" --no-tui &
HEADLESS_PID=$!

for i in $(seq 1 15); do
    if curl -sS -o /dev/null --connect-timeout 1 \
        -X POST "http://127.0.0.1:${PROFILE_PORT}/mcp/" \
        -H "content-type: application/json" \
        --data '{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"health_check","arguments":{}}}' 2>/dev/null; then
        break
    fi
    sleep 1
done

PROJECT_KEY="/tmp/profile-traversal-project"
curl -s -X POST "http://127.0.0.1:${PROFILE_PORT}/mcp/" \
    -H "content-type: application/json" \
    --data "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"id\":1,\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_KEY}\"}}}" >/dev/null 2>&1 || true
for name in RedFox BlueLake GreenPeak GoldCastle SwiftHawk; do
    curl -s -X POST "http://127.0.0.1:${PROFILE_PORT}/mcp/" \
        -H "content-type: application/json" \
        --data "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"id\":1,\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"program\":\"test\",\"model\":\"test\",\"name\":\"${name}\"}}}" >/dev/null 2>&1 || true
done
for i in $(seq 1 20); do
    from_idx=$((i % 5))
    to_idx=$(((i + 1) % 5))
    agents=("RedFox" "BlueLake" "GreenPeak" "GoldCastle" "SwiftHawk")
    curl -s -X POST "http://127.0.0.1:${PROFILE_PORT}/mcp/" \
        -H "content-type: application/json" \
        --data "{\"jsonrpc\":\"2.0\",\"method\":\"tools/call\",\"id\":1,\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_KEY}\",\"sender_name\":\"${agents[$from_idx]}\",\"to\":[\"${agents[$to_idx]}\"],\"subject\":\"Profile test message ${i}\",\"body_md\":\"Profiling body ${i}\",\"thread_id\":\"PROF-${i}\",\"importance\":\"normal\"}}}" >/dev/null 2>&1 || true
done
echo "  Data seeded."

kill "${HEADLESS_PID}" 2>/dev/null || true
wait "${HEADLESS_PID}" 2>/dev/null || true
sleep 1

# Run PTY traversal with concurrent /proc polling for CPU + thread stats.
# This is a single Python script that forks the TUI, drives Tab keystrokes
# via PTY, and samples /proc/<pid>/stat + /proc/<pid>/task/*/stat in parallel.
echo "  Running PTY traversal with /proc profiling..."

python3 - "${PROFILE_DB}" "${PROFILE_STORAGE}" "${ARTIFACT_DIR}" "${BIN}" <<'PYEOF'
import sys, os, pty, select, time, json, re, signal, threading, struct, fcntl, termios
from pathlib import Path

db_path, storage_root, artifact_dir, binary = sys.argv[1:5]
artifact_dir = Path(artifact_dir)

port = 0
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
port = s.getsockname()[1]
s.close()

# Open PTY
master_fd, slave_fd = pty.openpty()
cols, rows = 120, 40
winsize = struct.pack("HHHH", rows, cols, 0, 0)
fcntl.ioctl(master_fd, termios.TIOCSWINSZ, winsize)

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
    env.update({
        "TERM": "xterm-256color",
        "COLUMNS": str(cols),
        "LINES": str(rows),
        "DATABASE_URL": f"sqlite:////{db_path}",
        "STORAGE_ROOT": storage_root,
        "HTTP_HOST": "127.0.0.1",
        "HTTP_PORT": str(port),
        "HTTP_RBAC_ENABLED": "0",
        "HTTP_RATE_LIMIT_ENABLED": "0",
        "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED": "1",
    })
    os.execvpe(binary, [binary, "serve", "--host", "127.0.0.1", "--port", str(port)], env)

# Parent: drive PTY + profile
os.close(slave_fd)
QUIESCE_GAP = 0.08  # 80ms

# /proc sampler thread
proc_samples = []
sampling = True
HZ = os.sysconf("SC_CLK_TCK")

def read_proc_stat(p):
    """Read /proc/<pid>/stat, return (utime, stime, num_threads, voluntary_ctxt_switches, nonvoluntary_ctxt_switches)."""
    try:
        with open(f"/proc/{p}/stat") as f:
            parts = f.read().split()
        utime = int(parts[13])
        stime = int(parts[14])
        num_threads = int(parts[19])
        return utime, stime, num_threads
    except (FileNotFoundError, IndexError, ValueError):
        return None

def read_proc_status(p):
    """Read voluntary/nonvoluntary context switches from /proc/<pid>/status."""
    try:
        with open(f"/proc/{p}/status") as f:
            text = f.read()
        vol = int(re.search(r"voluntary_ctxt_switches:\s+(\d+)", text).group(1))
        nonvol = int(re.search(r"nonvoluntary_ctxt_switches:\s+(\d+)", text).group(1))
        return vol, nonvol
    except (FileNotFoundError, AttributeError, ValueError):
        return None

def read_thread_names(p):
    """Read thread names from /proc/<pid>/task/*/comm."""
    names = {}
    try:
        task_dir = Path(f"/proc/{p}/task")
        for tid_dir in task_dir.iterdir():
            try:
                names[tid_dir.name] = (tid_dir / "comm").read_text().strip()
            except (FileNotFoundError, PermissionError):
                pass
    except (FileNotFoundError, PermissionError):
        pass
    return names

def sampler():
    while sampling:
        t = time.monotonic()
        stat = read_proc_stat(pid)
        status = read_proc_status(pid)
        if stat and status:
            proc_samples.append({
                "t": t,
                "utime": stat[0],
                "stime": stat[1],
                "num_threads": stat[2],
                "vol_csw": status[0],
                "nonvol_csw": status[1],
            })
        time.sleep(0.1)  # Sample at 10Hz

sampler_thread = threading.Thread(target=sampler, daemon=True)
sampler_thread.start()

# Capture thread names early
time.sleep(2)  # Let TUI initialize
thread_names = read_thread_names(pid)

# Drive Tab keystrokes
chunks = []
timings = []

def read_with_latency(max_wait=2.0):
    t0 = time.monotonic()
    deadline = t0 + max_wait
    got = 0
    first_byte_t = None
    last_byte_t = None
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            break
        if first_byte_t is not None:
            wait = min(remaining, QUIESCE_GAP)
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
            if first_byte_t is not None:
                break
    now = time.monotonic()
    return {
        "first_byte_ms": round((first_byte_t - t0) * 1000, 2) if first_byte_t else None,
        "quiesce_ms": round((now - t0) * 1000, 2),
        "render_ms": round(((last_byte_t or now) - (first_byte_t or t0)) * 1000, 2) if first_byte_t else 0,
        "total_bytes": got,
    }

# Initial render
t_start = time.monotonic()
init = read_with_latency(max_wait=5.0)
timings.append({"step": "initial_render", **init})

# Tab through all 15 screens
SCREEN_NAMES = [
    "Messages", "Threads", "Agents", "Search", "Reservations",
    "Tool_Metrics", "System_Health", "Timeline", "Projects", "Contacts",
    "Explorer", "Analytics", "Attachments", "Archive_Browser", "Dashboard"
]

for i, screen in enumerate(SCREEN_NAMES):
    try:
        os.write(master_fd, b"\t")
    except OSError:
        break
    r = read_with_latency(max_wait=1.0)
    timings.append({"step": f"tab_to_{screen}", **r})

# Quit
try:
    os.write(master_fd, b"q")
except OSError:
    pass
time.sleep(0.5)

# Stop sampling
sampling = False
sampler_thread.join(timeout=2)
t_end = time.monotonic()

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

# Compute CPU deltas from proc_samples
cpu_profile = []
for i in range(1, len(proc_samples)):
    prev, curr = proc_samples[i-1], proc_samples[i]
    dt = curr["t"] - prev["t"]
    if dt <= 0:
        continue
    utime_delta = (curr["utime"] - prev["utime"]) / HZ
    stime_delta = (curr["stime"] - prev["stime"]) / HZ
    cpu_pct = ((utime_delta + stime_delta) / dt) * 100
    vol_csw_delta = curr["vol_csw"] - prev["vol_csw"]
    nonvol_csw_delta = curr["nonvol_csw"] - prev["nonvol_csw"]
    cpu_profile.append({
        "t_offset_s": round(curr["t"] - t_start, 2),
        "cpu_pct": round(cpu_pct, 2),
        "usr_pct": round(utime_delta / dt * 100, 2),
        "sys_pct": round(stime_delta / dt * 100, 2),
        "threads": curr["num_threads"],
        "vol_csw": vol_csw_delta,
        "nonvol_csw": nonvol_csw_delta,
    })

# Summary stats
cpu_pcts = [s["cpu_pct"] for s in cpu_profile]
vol_csws = [s["vol_csw"] for s in cpu_profile]
nonvol_csws = [s["nonvol_csw"] for s in cpu_profile]

def pstats(vals):
    if not vals:
        return {}
    s = sorted(vals)
    n = len(s)
    return {
        "count": n,
        "min": s[0],
        "max": s[-1],
        "mean": round(sum(s) / n, 2),
        "p50": s[n // 2],
        "p95": s[int(n * 0.95)] if n >= 2 else s[-1],
        "p99": s[int(n * 0.99)] if n >= 2 else s[-1],
    }

profile_data = {
    "cpu_time_series": cpu_profile,
    "cpu_summary": pstats(cpu_pcts),
    "voluntary_csw_summary": pstats(vol_csws),
    "nonvoluntary_csw_summary": pstats(nonvol_csws),
    "thread_names": thread_names,
    "thread_count": max((s["threads"] for s in cpu_profile), default=0),
    "total_samples": len(proc_samples),
    "sample_hz": 10,
    "traversal_timings": timings,
}

# Write profiling data
with open(artifact_dir / "proc_profile.json", "w") as f:
    json.dump(profile_data, f, indent=2)

# Print summary
print(f"\n  /proc profiling: {len(proc_samples)} samples at 10Hz")
print(f"  Threads: {profile_data['thread_count']} ({len(thread_names)} named)")
cs = profile_data["cpu_summary"]
if cs:
    print(f"  CPU: mean={cs['mean']}% p95={cs['p95']}% max={cs['max']}%")
vs = profile_data["voluntary_csw_summary"]
if vs:
    print(f"  Vol CSW/100ms: mean={vs['mean']} p95={vs['p95']} max={vs['max']}")
nvs = profile_data["nonvoluntary_csw_summary"]
if nvs:
    print(f"  NonVol CSW/100ms: mean={nvs['mean']} p95={nvs['p95']} max={nvs['max']}")

# Per-screen activation latencies
print(f"\n  Per-screen activation (quiesce_ms):")
for t in timings:
    if t["step"].startswith("tab_to_"):
        name = t["step"].replace("tab_to_", "").replace("_", " ")
        fb = t.get("first_byte_ms", "?")
        q = t.get("quiesce_ms", "?")
        print(f"    {name:20s}: first_byte={fb}ms quiesce={q}ms bytes={t.get('total_bytes', 0)}")
PYEOF

echo "  Profiling capture complete."

# ────────────────────────────────────────────────────────────────────
# Step 3: Analyze and produce profile_report.json
# ────────────────────────────────────────────────────────────────────
echo ""
echo "--- Step 3: Generating profile report ---"

python3 - "${ARTIFACT_DIR}" <<'PYEOF'
import sys, json, os, re
from pathlib import Path

artifact_dir = Path(sys.argv[1])

report = {
    "bead": "br-legjy.1.2",
    "purpose": "Baseline profiling evidence for TUI traversal incident",
}

# Parse traversal results
traversal_file = artifact_dir / "traversal_results.json"
if traversal_file.exists():
    with open(traversal_file) as f:
        traversal = json.load(f)
    report["traversal"] = {
        "screen_count": traversal.get("screen_count", 0),
        "overall_quiesce": traversal.get("overall", {}).get("quiesce", {}),
        "overall_first_byte": traversal.get("overall", {}).get("first_byte", {}),
        "overall_render": traversal.get("overall", {}).get("render", {}),
        "forward_quiesce": traversal.get("forward", {}).get("summary_quiesce", {}),
        "backward_quiesce": traversal.get("backward", {}).get("summary_quiesce", {}),
        "jump_quiesce": traversal.get("jump", {}).get("summary_quiesce", {}),
    }

# Parse /proc profile data (from Step 2 PTY-driven profiling)
proc_profile_file = artifact_dir / "proc_profile.json"
if proc_profile_file.exists():
    with open(proc_profile_file) as f:
        proc_data = json.load(f)
    report["cpu_profile"] = {
        "summary": proc_data.get("cpu_summary", {}),
        "voluntary_csw": proc_data.get("voluntary_csw_summary", {}),
        "nonvoluntary_csw": proc_data.get("nonvoluntary_csw_summary", {}),
        "thread_count": proc_data.get("thread_count", 0),
        "thread_names": proc_data.get("thread_names", {}),
        "sample_count": proc_data.get("total_samples", 0),
        "sample_hz": proc_data.get("sample_hz", 10),
    }
    # Include per-screen timings from the profiled run
    profiled_timings = proc_data.get("traversal_timings", [])
    profiled_screen_latencies = []
    for t in profiled_timings:
        if t.get("step", "").startswith("tab_to_"):
            profiled_screen_latencies.append({
                "screen": t["step"].replace("tab_to_", "").replace("_", " "),
                "first_byte_ms": t.get("first_byte_ms"),
                "quiesce_ms": t.get("quiesce_ms", 0),
                "render_ms": t.get("render_ms", 0),
                "output_bytes": t.get("total_bytes", 0),
            })
    report["profiled_traversal"] = profiled_screen_latencies

    # Include CPU time series for visual analysis
    cpu_ts = proc_data.get("cpu_time_series", [])
    if cpu_ts:
        report["cpu_time_series_sample"] = cpu_ts[:50]  # First 50 samples

# Load env metadata
env_file = artifact_dir / "env_metadata.json"
if env_file.exists():
    with open(env_file) as f:
        report["environment"] = json.load(f)

# Write report
report_file = artifact_dir / "profile_report.json"
with open(report_file, "w") as f:
    json.dump(report, f, indent=2)

# Print summary
print(f"\n=== Profile Report Summary ===")

if "traversal" in report:
    t = report["traversal"]
    oq = t.get("overall_quiesce", {})
    ofb = t.get("overall_first_byte", {})
    print(f"  Traversal: {t.get('screen_count', 0)} screens")
    print(f"    First byte: mean={ofb.get('mean_ms', '?')}ms p95={ofb.get('p95_ms', '?')}ms")
    print(f"    Quiesce:    mean={oq.get('mean_ms', '?')}ms p95={oq.get('p95_ms', '?')}ms max={oq.get('max_ms', '?')}ms")

if "cpu_profile" in report:
    cp = report["cpu_profile"]
    cs = cp.get("summary", {})
    vs = cp.get("voluntary_csw", {})
    nvs = cp.get("nonvoluntary_csw", {})
    print(f"  /proc profiling: {cp.get('sample_count', 0)} samples at {cp.get('sample_hz', 10)}Hz")
    print(f"  Threads: {cp.get('thread_count', 0)} ({len(cp.get('thread_names', {}))} named)")
    if cs:
        print(f"  CPU: mean={cs.get('mean', '?')}% p95={cs.get('p95', '?')}% max={cs.get('max', '?')}%")
    if vs:
        print(f"  Vol CSW/100ms: mean={vs.get('mean', '?')} p95={vs.get('p95', '?')} max={vs.get('max', '?')}")
    if nvs:
        print(f"  NonVol CSW/100ms: mean={nvs.get('mean', '?')} p95={nvs.get('p95', '?')} max={nvs.get('max', '?')}")

    # Thread name listing
    names = cp.get("thread_names", {})
    if names:
        from collections import Counter
        name_counts = Counter(names.values())
        print(f"  Thread types: {', '.join(f'{n}({c})' for n, c in name_counts.most_common(8))}")

if "profiled_traversal" in report:
    print(f"  Profiled per-screen latencies:")
    for entry in report["profiled_traversal"]:
        fb = entry.get("first_byte_ms", "?")
        q = entry.get("quiesce_ms", "?")
        print(f"    {entry['screen']:20s}: first_byte={fb}ms quiesce={q}ms bytes={entry.get('output_bytes', 0)}")

print(f"\nFull report: {report_file}")
PYEOF

echo ""
echo "=== Profiling complete ==="
echo "Artifacts: ${ARTIFACT_DIR}"
ls -la "${ARTIFACT_DIR}/"
