#!/usr/bin/env bash
# incident_capture_harness.sh
#
# One-command deterministic incident repro/capture harness for br-2k3qx.1.3.
#
# Workflow:
# 1. Seeds high-cardinality fixture via seed_truth_incident_fixture.sh
# 2. Launches headless server in deterministic mode
# 3. Captures key surface snapshots (dashboard/messages/threads/agents/projects/health)
# 4. Queries SQLite directly for DB truth diagnostics
# 5. Emits context + manifest metadata
# 6. Bundles everything as a CI-consumable artifact
#
# Exit codes:
#   0  capture completed, all surfaces match DB truth
#   1  fatal error (seeding, server startup, missing commands)
#   2  capture completed but verification failures detected (truth mismatches and/or capture failures)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SEED_SCRIPT="${SCRIPT_DIR}/seed_truth_incident_fixture.sh"
source "${SCRIPT_DIR}/truth_oracle_lib.sh"
TRUTH_ORACLE_LOG_PREFIX="incident-capture"

DEFAULT_PROJECTS=15
DEFAULT_AGENTS=300
DEFAULT_MESSAGES=3200
DEFAULT_THREADS=320
DEFAULT_SEED=20260302

OUTPUT_DIR="${PROJECT_ROOT}/tests/artifacts/incident_capture/$(date -u '+%Y%m%d_%H%M%S')"
DB_PATH=""
STORAGE_ROOT=""
PORT=""
SEED="${DEFAULT_SEED}"
PROJECT_COUNT="${DEFAULT_PROJECTS}"
AGENT_COUNT="${DEFAULT_AGENTS}"
MESSAGE_COUNT="${DEFAULT_MESSAGES}"
THREAD_COUNT="${DEFAULT_THREADS}"
KEEP_SERVER=0
VERBOSE=0
VERIFY_DETERMINISM=0
ROBOT_TIMEOUT_SECS=12
SKIP_HTTP_SERVER=0

SERVER_PID=""
SERVER_LOG=""
PROJECT_KEY=""
AGENT_NAME=""
THREAD_ID=""
MESSAGE_ID=""
FIXTURE_FINGERPRINT=""
CAPTURE_FAILURES=0
CAPTURE_SUCCESSES=0
TRUTH_MISMATCHES=0
TRUTH_INDETERMINATE=0

usage() {
    cat <<'USAGE'
Usage: scripts/incident_capture_harness.sh [options]

Options:
  --output-dir <path>       Artifact output directory
  --db <path>               SQLite DB path (default: <output-dir>/incident_capture.sqlite3)
  --storage-root <path>     Storage root path (default: <output-dir>/storage_root)
  --seed <n>                Deterministic seed (default: 20260302)
  --projects <n>            Project count (default: 15)
  --agents <n>              Agent count (default: 300)
  --messages <n>            Message count (default: 3200)
  --threads <n>             Thread count (default: 320)
  --port <n>                HTTP port (default: auto-selected free port)
  --robot-timeout-secs <n>  Timeout per am robot command (default: 12)
  --skip-http-server        Skip headless HTTP server startup; write fallback HTTP snapshots
  --keep-server             Do not stop server at end (prints PID/url)
  --verify-determinism      Re-seed and verify fingerprint matches
  --verbose                 Verbose logging
  -h, --help                Show help

Outputs:
  <output-dir>/
    fixture_report.json               # Seed validation report with SHA256 fingerprint
    fixture_seed_stdout.json           # Raw seeder output
    fixture_seed_stderr.log            # Seeder diagnostics
    context.json                       # Harness metadata (paths, IDs, seed)
    repro.sh                           # One-liner to reproduce this exact capture
    diagnostics/
      db_counts.tsv                    # Raw SQLite counts
      db_truth.json                    # Structured DB truth (counts + per-project)
      truth_comparison.json            # DB truth vs surface truth comparison
    logs/
      server.log                       # Headless server output
    snapshots/
      dashboard_status.json            # am robot status
      messages_inbox.json              # am robot inbox --all
      message_detail.json              # am robot message <id>
      threads_view.md                  # am robot thread <id> --format md
      agents_view.json                 # am robot agents
      projects_view.json               # am robot projects
      system_health.json               # am robot health
      http_health.json                 # curl /health
      mail_root.html                   # curl /mail/
      mail_root_headers.txt            # HTTP headers from /mail/
    incident_capture_manifest.json     # Full file listing
    incident_capture_bundle.tar.gz     # Compressed archive of all artifacts
USAGE
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

wait_for_health() {
    local port="$1"
    local attempts="${2:-120}"
    local delay="${3:-0.1}"
    local url="http://127.0.0.1:${port}/health"
    local i
    for i in $(seq 1 "${attempts}"); do
        if curl -fsS -o /dev/null --connect-timeout 1 --max-time 1 "${url}" 2>/dev/null; then
            return 0
        fi
        if [ -n "${SERVER_PID}" ] && ! kill -0 "${SERVER_PID}" 2>/dev/null; then
            return 1
        fi
        sleep "${delay}"
    done
    return 1
}

write_http_capture_fallback() {
    local reason="$1"
    printf '{"_capture_error":true,"label":"http_health","reason":"%s"}\n' "${reason}" > "${OUTPUT_DIR}/snapshots/http_health.json"
    cat > "${OUTPUT_DIR}/snapshots/mail_root.html" <<HTML
<!doctype html>
<html><body><p>capture_error: ${reason}</p></body></html>
HTML
    printf 'capture_error: %s\n' "${reason}" > "${OUTPUT_DIR}/snapshots/mail_root_headers.txt"
}

cleanup() {
    if [ "${KEEP_SERVER}" -eq 1 ]; then
        return 0
    fi
    if [ -n "${SERVER_PID}" ] && kill -0 "${SERVER_PID}" 2>/dev/null; then
        kill "${SERVER_PID}" >/dev/null 2>&1 || true
        wait "${SERVER_PID}" >/dev/null 2>&1 || true
    fi
}
trap cleanup EXIT

# Fault-tolerant robot capture: records success/failure but never dies.
# Writes fallback payloads on failure so downstream artifact parsers stay robust.
run_robot_capture() {
    local output_path="$1"
    shift
    local label
    label="$(basename "${output_path}")"
    local stderr_path="${output_path}.stderr"
    local exit_code=0
    if command -v timeout >/dev/null 2>&1; then
        AM_INTERFACE_MODE=cli \
            DATABASE_URL="sqlite:///${DB_PATH}" \
            STORAGE_ROOT="${STORAGE_ROOT}" \
            timeout --kill-after=3s "${ROBOT_TIMEOUT_SECS}s" \
            am robot --project "${PROJECT_KEY}" --agent "${AGENT_NAME}" "$@" >"${output_path}" 2>"${stderr_path}" || exit_code=$?
    else
        AM_INTERFACE_MODE=cli \
            DATABASE_URL="sqlite:///${DB_PATH}" \
            STORAGE_ROOT="${STORAGE_ROOT}" \
            am robot --project "${PROJECT_KEY}" --agent "${AGENT_NAME}" "$@" >"${output_path}" 2>"${stderr_path}" || exit_code=$?
    fi
    if [ "${exit_code}" -ne 0 ]; then
        CAPTURE_FAILURES=$((CAPTURE_FAILURES + 1))
        log "FAIL: robot ${label} (exit ${exit_code})"
        if [[ "${output_path}" == *.md ]]; then
            {
                printf '# Capture Failure: %s\n\n' "${label}"
                printf '- command: `am robot --project %s --agent %s %s`\n' "${PROJECT_KEY}" "${AGENT_NAME}" "$*"
                printf '- exit_code: %d\n\n' "${exit_code}"
                printf '```\n'
                sed -n '1,120p' "${stderr_path}" 2>/dev/null || true
                printf '\n```\n'
            } > "${output_path}"
        else
            # Write a sentinel so jq pipelines don't choke
            printf '{"_capture_error": true, "label": "%s", "exit_code": %d}\n' "${label}" "${exit_code}" > "${output_path}"
        fi
    else
        CAPTURE_SUCCESSES=$((CAPTURE_SUCCESSES + 1))
        log "OK: robot ${label}"
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --output-dir)
            OUTPUT_DIR="${2:-}"
            shift 2
            ;;
        --db)
            DB_PATH="${2:-}"
            shift 2
            ;;
        --storage-root)
            STORAGE_ROOT="${2:-}"
            shift 2
            ;;
        --seed)
            SEED="${2:-}"
            shift 2
            ;;
        --projects)
            PROJECT_COUNT="${2:-}"
            shift 2
            ;;
        --agents)
            AGENT_COUNT="${2:-}"
            shift 2
            ;;
        --messages)
            MESSAGE_COUNT="${2:-}"
            shift 2
            ;;
        --threads)
            THREAD_COUNT="${2:-}"
            shift 2
            ;;
        --port)
            PORT="${2:-}"
            shift 2
            ;;
        --robot-timeout-secs)
            ROBOT_TIMEOUT_SECS="${2:-}"
            shift 2
            ;;
        --skip-http-server)
            SKIP_HTTP_SERVER=1
            shift
            ;;
        --keep-server)
            KEEP_SERVER=1
            shift
            ;;
        --verify-determinism)
            VERIFY_DETERMINISM=1
            shift
            ;;
        --verbose)
            VERBOSE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            usage
            die "unknown argument: $1"
            ;;
    esac
done

require_int_vars SEED PROJECT_COUNT AGENT_COUNT MESSAGE_COUNT THREAD_COUNT ROBOT_TIMEOUT_SECS
require_port_or_empty PORT "${PORT}"

require_cmds mcp-agent-mail am sqlite3 python3 jq curl
[ -x "${SEED_SCRIPT}" ] || die "seed script not found/executable: ${SEED_SCRIPT}"

mkdir -p "${OUTPUT_DIR}" "${OUTPUT_DIR}/snapshots" "${OUTPUT_DIR}/logs" "${OUTPUT_DIR}/diagnostics"

if [ -z "${DB_PATH}" ]; then
    DB_PATH="${OUTPUT_DIR}/incident_capture.sqlite3"
fi
if [ -z "${STORAGE_ROOT}" ]; then
    STORAGE_ROOT="${OUTPUT_DIR}/storage_root"
fi
mkdir -p "$(dirname "${DB_PATH}")" "${STORAGE_ROOT}"

if [ -z "${PORT}" ]; then
    PORT="$(pick_port)"
fi
SERVER_LOG="${OUTPUT_DIR}/logs/server.log"

log "seeding deterministic fixture"
if ! "${SEED_SCRIPT}" \
    --db "${DB_PATH}" \
    --storage-root "${STORAGE_ROOT}" \
    --report "${OUTPUT_DIR}/fixture_report.json" \
    --projects "${PROJECT_COUNT}" \
    --agents "${AGENT_COUNT}" \
    --messages "${MESSAGE_COUNT}" \
    --threads "${THREAD_COUNT}" \
    --seed "${SEED}" \
    --overwrite \
    >"${OUTPUT_DIR}/fixture_seed_stdout.json" \
    2>"${OUTPUT_DIR}/fixture_seed_stderr.log"; then
    sed -n '1,200p' "${OUTPUT_DIR}/fixture_seed_stderr.log" >&2 || true
    die "fixture seeding failed"
fi

if ! jq -e '.ok == true' "${OUTPUT_DIR}/fixture_report.json" >/dev/null 2>&1; then
    die "fixture report indicates failure"
fi
FIXTURE_FINGERPRINT="$(jq -r '.dataset_fingerprint_sha256' "${OUTPUT_DIR}/fixture_report.json")"

if [ "${VERIFY_DETERMINISM}" -eq 1 ]; then
    log "verifying deterministic seed reproducibility"
    DET_REPORT="${OUTPUT_DIR}/diagnostics/determinism_rerun_report.json"
    DET_STDOUT="${OUTPUT_DIR}/diagnostics/determinism_rerun_stdout.json"
    DET_STDERR="${OUTPUT_DIR}/diagnostics/determinism_rerun_stderr.log"
    if ! "${SEED_SCRIPT}" \
        --db "${DB_PATH}" \
        --storage-root "${STORAGE_ROOT}" \
        --report "${DET_REPORT}" \
        --projects "${PROJECT_COUNT}" \
        --agents "${AGENT_COUNT}" \
        --messages "${MESSAGE_COUNT}" \
        --threads "${THREAD_COUNT}" \
        --seed "${SEED}" \
        --overwrite \
        >"${DET_STDOUT}" \
        2>"${DET_STDERR}"; then
        sed -n '1,200p' "${DET_STDERR}" >&2 || true
        die "determinism rerun failed"
    fi
    DET_FINGERPRINT="$(jq -r '.dataset_fingerprint_sha256' "${DET_REPORT}")"
    python3 - "${OUTPUT_DIR}/diagnostics/determinism_check.json" "${FIXTURE_FINGERPRINT}" "${DET_FINGERPRINT}" <<'PY'
import json
import sys

path, first_fp, rerun_fp = sys.argv[1:4]
payload = {
    "first_fingerprint_sha256": first_fp,
    "rerun_fingerprint_sha256": rerun_fp,
    "match": first_fp == rerun_fp,
}
with open(path, "w", encoding="utf-8") as f:
    json.dump(payload, f, indent=2, sort_keys=True)
PY
    [ "${DET_FINGERPRINT}" = "${FIXTURE_FINGERPRINT}" ] || die "determinism check failed (fingerprint mismatch)"
fi

PROJECT_KEY="$(sqlite3 "${DB_PATH}" "SELECT human_key FROM projects ORDER BY id LIMIT 1;")"
AGENT_NAME="$(sqlite3 "${DB_PATH}" "SELECT name FROM agents WHERE project_id = (SELECT id FROM projects ORDER BY id LIMIT 1) ORDER BY id LIMIT 1;")"
THREAD_ID="$(sqlite3 "${DB_PATH}" "SELECT thread_id FROM messages WHERE thread_id IS NOT NULL AND thread_id != '' ORDER BY id LIMIT 1;")"
MESSAGE_ID="$(sqlite3 "${DB_PATH}" "SELECT id FROM messages ORDER BY id LIMIT 1;")"
require_non_empty "seeded project key" "${PROJECT_KEY}"
require_non_empty "seeded agent name" "${AGENT_NAME}"
require_non_empty "seeded thread id" "${THREAD_ID}"
require_non_empty "seeded message id" "${MESSAGE_ID}"

if [ "${SKIP_HTTP_SERVER}" -eq 1 ]; then
    log "skipping headless HTTP server startup (--skip-http-server)"
    : > "${SERVER_LOG}"
    write_http_capture_fallback "http_server_skipped"
else
    log "starting headless server on 127.0.0.1:${PORT}"
    DATABASE_URL="sqlite:///${DB_PATH}" \
    STORAGE_ROOT="${STORAGE_ROOT}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${PORT}" \
    HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=true \
    TUI_ENABLED=false \
    mcp-agent-mail serve --no-tui >"${SERVER_LOG}" 2>&1 &
    SERVER_PID=$!

    if ! wait_for_health "${PORT}" 300 0.1; then
        CAPTURE_FAILURES=$((CAPTURE_FAILURES + 1))
        log "WARN: server failed to become healthy; continuing with fallback HTTP snapshots"
        write_http_capture_fallback "server_unhealthy_timeout"
        if [ -n "${SERVER_PID}" ] && kill -0 "${SERVER_PID}" 2>/dev/null; then
            kill "${SERVER_PID}" >/dev/null 2>&1 || true
            wait "${SERVER_PID}" >/dev/null 2>&1 || true
        fi
        SERVER_PID=""
    else
        if ! curl -sS --max-time 8 "http://127.0.0.1:${PORT}/health" > "${OUTPUT_DIR}/snapshots/http_health.json"; then
            CAPTURE_FAILURES=$((CAPTURE_FAILURES + 1))
            printf '{"_capture_error":true,"label":"http_health","exit_code":1}\n' > "${OUTPUT_DIR}/snapshots/http_health.json"
        fi
        if ! curl -sS --max-time 10 -D "${OUTPUT_DIR}/snapshots/mail_root_headers.txt" \
            "http://127.0.0.1:${PORT}/mail/" > "${OUTPUT_DIR}/snapshots/mail_root.html"; then
            CAPTURE_FAILURES=$((CAPTURE_FAILURES + 1))
            write_http_capture_fallback "mail_root_timeout"
        fi
    fi
fi

# ---------------------------------------------------------------------------
# Phase 2: Capture all 16 robot command snapshots
# ---------------------------------------------------------------------------
log "capturing robot snapshots (all 16 commands)"

# Track 2: Situational Awareness
run_robot_capture "${OUTPUT_DIR}/snapshots/dashboard_status.json"  status --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/messages_inbox.json"    inbox --all --include-bodies --limit 30 --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/messages_inbox_limit5.json" inbox --all --include-bodies --limit 5 --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/messages_inbox_unread.json" inbox --all --unread --limit 30 --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/timeline.json"          timeline --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/overview.json"          overview --format json

# Track 3: Context & Discovery
run_robot_capture "${OUTPUT_DIR}/snapshots/threads_view.json"      thread "${THREAD_ID}" --limit 50 --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/threads_view_limit3.json" thread "${THREAD_ID}" --limit 3 --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/threads_view.md"        thread "${THREAD_ID}" --limit 50 --format md
run_robot_capture "${OUTPUT_DIR}/snapshots/message_detail.json"    message "${MESSAGE_ID}" --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/search_results.json"    search "truth-fixture" --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/search_results_since.json" search "truth-fixture" --since "1970-01-01T00:00:00Z" --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/search_results_kind_message.json" search "truth-fixture" --kind message --format json

# Track 3b: Navigation (resource:// URI resolution)
run_robot_capture "${OUTPUT_DIR}/snapshots/navigate.json"           navigate "resource://agents/${PROJECT_KEY}" --format json

# Track 4: Monitoring & Analytics
run_robot_capture "${OUTPUT_DIR}/snapshots/reservations.json"      reservations --all --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/metrics.json"           metrics --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/system_health.json"     health --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/analytics.json"         analytics --format json

# Track 5: Entity Views
run_robot_capture "${OUTPUT_DIR}/snapshots/agents_view.json"       agents --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/contacts.json"          contacts --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/projects_view.json"     projects --format json
run_robot_capture "${OUTPUT_DIR}/snapshots/attachments.json"       attachments --format json

log "robot snapshots: ${CAPTURE_SUCCESSES} succeeded, ${CAPTURE_FAILURES} failed"

# ---------------------------------------------------------------------------
# Phase 3: Extract structured DB truth
# ---------------------------------------------------------------------------
log "extracting DB truth"

if ! sqlite3 "${DB_PATH}" > "${OUTPUT_DIR}/diagnostics/db_counts.tsv" <<'SQL'
.mode tabs
SELECT 'projects', COUNT(*) FROM projects;
SELECT 'agents', COUNT(*) FROM agents;
SELECT 'messages', COUNT(*) FROM messages;
SELECT 'threads', COUNT(DISTINCT thread_id) FROM messages WHERE thread_id IS NOT NULL AND thread_id != '';
SELECT 'ack_required_messages', COUNT(*) FROM messages WHERE ack_required = 1;
SELECT 'recipient_rows', COUNT(*) FROM message_recipients;
SELECT 'recipient_reads', COUNT(*) FROM message_recipients WHERE read_ts IS NOT NULL;
SELECT 'recipient_acks', COUNT(*) FROM message_recipients WHERE ack_ts IS NOT NULL;
SQL
then
    CAPTURE_FAILURES=$((CAPTURE_FAILURES + 1))
    log "WARN: sqlite3 db_counts query failed; writing fallback counts from fixture report"
    python3 - "${OUTPUT_DIR}/fixture_report.json" "${OUTPUT_DIR}/diagnostics/db_counts.tsv" <<'PY'
import json
import sys

report_path, out_path = sys.argv[1:3]
with open(report_path, "r", encoding="utf-8") as f:
    report = json.load(f)

counts = report.get("counts", {})
state = report.get("state_mix", {})
rows = [
    ("projects", counts.get("projects", 0)),
    ("agents", counts.get("agents", 0)),
    ("messages", counts.get("messages", 0)),
    ("threads", counts.get("threads", 0)),
    ("ack_required_messages", state.get("ack_required_messages", 0)),
    ("recipient_rows", state.get("recipient_rows", 0)),
    ("recipient_reads", state.get("recipient_reads", 0)),
    ("recipient_acks", state.get("recipient_acks", 0)),
]
with open(out_path, "w", encoding="utf-8") as f:
    for key, value in rows:
        f.write(f"{key}\t{int(value)}\n")
PY
fi

# ---------------------------------------------------------------------------
# Phase 3b: Build structured DB truth JSON (global + per-project breakdown)
# ---------------------------------------------------------------------------
log "building structured db_truth.json"

python3 - "${DB_PATH}" "${OUTPUT_DIR}/diagnostics/db_truth.json" "${OUTPUT_DIR}/fixture_report.json" <<'DBTRUTH'
import json
import sqlite3
import sys

db_path, out_path, fixture_report_path = sys.argv[1:4]

def as_int(value, default=0):
    try:
        return int(value)
    except (TypeError, ValueError):
        return default

def split_evenly(total, buckets):
    if buckets <= 0:
        return []
    base = total // buckets
    rem = total % buckets
    return [base + (1 if i < rem else 0) for i in range(buckets)]

def build_fallback_per_project(project_count, agent_total, message_total, thread_total):
    per_project = {}
    if project_count <= 0:
        return per_project
    agents_split = split_evenly(agent_total, project_count)
    messages_split = split_evenly(message_total, project_count)
    threads_split = split_evenly(thread_total, project_count)
    for idx in range(project_count):
        key = f"/fallback/project/{idx + 1:03d}"
        per_project[key] = {
            "agents": agents_split[idx],
            "messages": messages_split[idx],
            "threads": threads_split[idx],
        }
    return per_project

fixture_report = {}
try:
    with open(fixture_report_path, "r", encoding="utf-8") as f:
        fixture_report = json.load(f)
except Exception:
    fixture_report = {}

conn = sqlite3.connect(db_path)
conn.row_factory = sqlite3.Row

# Global counts
global_counts = {}
for table, query in [
    ("projects",       "SELECT COUNT(*) AS n FROM projects"),
    ("agents",         "SELECT COUNT(*) AS n FROM agents"),
    ("messages",       "SELECT COUNT(*) AS n FROM messages"),
    ("threads",        "SELECT COUNT(DISTINCT thread_id) AS n FROM messages WHERE thread_id IS NOT NULL AND thread_id != ''"),
    ("ack_required",   "SELECT COUNT(*) AS n FROM messages WHERE ack_required = 1"),
    ("recipients",     "SELECT COUNT(*) AS n FROM message_recipients"),
    ("recipient_reads","SELECT COUNT(*) AS n FROM message_recipients WHERE read_ts IS NOT NULL"),
    ("recipient_acks", "SELECT COUNT(*) AS n FROM message_recipients WHERE ack_ts IS NOT NULL"),
    ("file_reservations", "SELECT COUNT(*) AS n FROM file_reservations"),
]:
    try:
        global_counts[table] = conn.execute(query).fetchone()[0]
    except Exception as exc:
        global_counts[table] = f"error: {exc}"

# Per-project breakdown (project_key → {agents, messages, threads})
per_project = {}
try:
    rows = conn.execute("""
        SELECT p.human_key,
               (SELECT COUNT(*) FROM agents a WHERE a.project_id = p.id) AS agents,
               (SELECT COUNT(*) FROM messages m WHERE m.project_id = p.id) AS messages,
               (SELECT COUNT(DISTINCT m.thread_id) FROM messages m
                WHERE m.project_id = p.id AND m.thread_id IS NOT NULL AND m.thread_id != '') AS threads
        FROM projects p ORDER BY p.id
    """).fetchall()
    for r in rows:
        per_project[r[0]] = {"agents": r[1], "messages": r[2], "threads": r[3]}
except Exception as exc:
    per_project["_error"] = str(exc)

conn.close()

fallback_active = False
fallback_reason = ""
query_errors = any(isinstance(v, str) and v.startswith("error:") for v in global_counts.values())
if query_errors or "_error" in per_project:
    fallback_active = True
    fallback_reason = "sqlite_db_truth_query_failed"
    counts = fixture_report.get("counts", {})
    state_mix = fixture_report.get("state_mix", {})
    projects_n = as_int(counts.get("projects"), 0)
    agents_n = as_int(counts.get("agents"), 0)
    messages_n = as_int(counts.get("messages"), 0)
    threads_n = as_int(counts.get("threads"), 0)

    global_counts = {
        "projects": projects_n,
        "agents": agents_n,
        "messages": messages_n,
        "threads": threads_n,
        "ack_required": as_int(state_mix.get("ack_required_messages"), 0),
        "recipients": as_int(state_mix.get("recipient_rows"), 0),
        "recipient_reads": as_int(state_mix.get("recipient_reads"), 0),
        "recipient_acks": as_int(state_mix.get("recipient_acks"), 0),
        "file_reservations": 0,
    }
    per_project = build_fallback_per_project(projects_n, agents_n, messages_n, threads_n)

payload = {
    "global_counts": global_counts,
    "per_project": per_project,
    "fallback_active": fallback_active,
    "fallback_reason": fallback_reason,
}
with open(out_path, "w", encoding="utf-8") as f:
    json.dump(payload, f, indent=2, sort_keys=True)
DBTRUTH

# ---------------------------------------------------------------------------
# Phase 4: Truth comparison — DB truth vs surface (robot) outputs
# ---------------------------------------------------------------------------
log "comparing DB truth vs surface snapshots"

python3 - "${OUTPUT_DIR}" <<'COMPARE'
import json
import os
import sys
from typing import Any

out_dir = sys.argv[1]
diag_dir = os.path.join(out_dir, "diagnostics")
snap_dir = os.path.join(out_dir, "snapshots")

def safe_load(path):
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return None

def safe_int(v):
    try:
        return int(v)
    except (TypeError, ValueError):
        return None

def first_int(*values):
    for value in values:
        parsed = safe_int(value)
        if parsed is not None:
            return parsed
    return None

def compute_severity(db_truth: int, surface: int, comparator: str) -> str:
    if comparator == "lte":
        overflow = max(0, surface - db_truth)
        if db_truth == 0 and surface > 0:
            return "high"
        ratio = overflow / max(db_truth, 1)
        if ratio >= 0.50:
            return "high"
        if ratio >= 0.10:
            return "medium"
        return "low"

    if comparator == "gte":
        deficit = max(0, db_truth - surface)
        if db_truth > 0 and surface == 0:
            return "critical"
        ratio = deficit / max(db_truth, 1)
        if ratio >= 0.50:
            return "high"
        if ratio >= 0.10:
            return "medium"
        return "low"

    if db_truth > 0 and surface == 0:
        return "critical"
    delta = abs(db_truth - surface)
    ratio = delta / max(db_truth, 1)
    if ratio >= 0.50:
        return "high"
    if ratio >= 0.10:
        return "medium"
    return "low"

def build_check(
    *,
    surface_id: str,
    metric: str,
    comparator: str,
    db_truth: int | None,
    surface_value: int | None,
    snapshot: str,
    note: str | None = None,
) -> dict[str, Any]:
    matched: bool | None = None
    status = "indeterminate"
    severity = "none"
    if db_truth is not None and surface_value is not None:
        if comparator == "eq":
            matched = (db_truth == surface_value)
        elif comparator == "lte":
            matched = (surface_value <= db_truth)
        elif comparator == "gte":
            matched = (surface_value >= db_truth)
        if matched is True:
            status = "match"
        elif matched is False:
            status = "mismatch"
            severity = compute_severity(db_truth, surface_value, comparator)

    return {
        "check_id": f"{surface_id}:{metric}",
        "mode": "incident_capture",
        "surface_id": surface_id,
        "metric": metric,
        "comparator": comparator,
        "db_truth": db_truth,
        "surface_value": surface_value,
        "matched": matched,
        "status": status,
        "severity": severity,
        "snapshot": snapshot,
        "repro": {
            "mode": "incident_capture",
            "mode_output_dir": out_dir,
            "repro_script": os.path.join(out_dir, "repro.sh"),
            "surface_id": surface_id,
        },
        "note": note,
    }

db_truth = safe_load(os.path.join(diag_dir, "db_truth.json")) or {}
gc = db_truth.get("global_counts", {})
fallback_active = bool(db_truth.get("fallback_active"))
fallback_note = "db_truth fallback active (fixture_report-based counts); mismatch gating disabled"

def list_len(value):
    if isinstance(value, list):
        return len(value)
    return None

def min_required(total: int | None) -> int:
    return 1 if (total is not None and total > 0) else 0

def has_capture_error(payload):
    return isinstance(payload, dict) and bool(payload.get("_capture_error"))

def record_check(check):
    checks.append(check)
    if check["status"] == "mismatch":
        mismatches.append(check)
    elif check["status"] == "indeterminate":
        indeterminate_checks.append(check)

db_projects = None if fallback_active else safe_int(gc.get("projects"))
db_agents = None if fallback_active else safe_int(gc.get("agents"))
db_messages = None if fallback_active else safe_int(gc.get("messages"))
db_threads = None if fallback_active else safe_int(gc.get("threads"))

checks = []
mismatches = []
indeterminate_checks = []

dashboard_snap = safe_load(os.path.join(snap_dir, "dashboard_status.json"))
projects_snap = safe_load(os.path.join(snap_dir, "projects_view.json"))
agents_snap = safe_load(os.path.join(snap_dir, "agents_view.json"))
inbox_snap = safe_load(os.path.join(snap_dir, "messages_inbox.json"))
health_snap = safe_load(os.path.join(snap_dir, "system_health.json"))
threads_snap = safe_load(os.path.join(snap_dir, "threads_view.json"))
threads_limit3_snap = safe_load(os.path.join(snap_dir, "threads_view_limit3.json"))
search_snap = safe_load(os.path.join(snap_dir, "search_results.json"))
search_since_snap = safe_load(os.path.join(snap_dir, "search_results_since.json"))
search_kind_message_snap = safe_load(os.path.join(snap_dir, "search_results_kind_message.json"))
overview_snap = safe_load(os.path.join(snap_dir, "overview.json"))
attachments_snap = safe_load(os.path.join(snap_dir, "attachments.json"))
inbox_limit5_snap = safe_load(os.path.join(snap_dir, "messages_inbox_limit5.json"))
inbox_unread_snap = safe_load(os.path.join(snap_dir, "messages_inbox_unread.json"))

required_snapshots = {
    "dashboard_status.json": dashboard_snap,
    "projects_view.json": projects_snap,
    "agents_view.json": agents_snap,
    "messages_inbox.json": inbox_snap,
    "messages_inbox_limit5.json": inbox_limit5_snap,
    "messages_inbox_unread.json": inbox_unread_snap,
    "threads_view.json": threads_snap,
    "threads_view_limit3.json": threads_limit3_snap,
    "search_results.json": search_snap,
    "search_results_since.json": search_since_snap,
    "search_results_kind_message.json": search_kind_message_snap,
    "overview.json": overview_snap,
    "attachments.json": attachments_snap,
    "system_health.json": health_snap,
}

for snapshot_name, payload in required_snapshots.items():
    if payload is None:
        note = "snapshot missing or invalid JSON"
        present = 0
    elif has_capture_error(payload):
        note = "snapshot capture_error"
        present = 0
    else:
        note = None
        present = 1
    record_check(build_check(
        surface_id="robot.capture",
        metric=f"{snapshot_name}.present",
        comparator="eq",
        db_truth=1,
        surface_value=present,
        snapshot=snapshot_name,
        note=note,
    ))

base_inbox_count = None
base_thread_count = None
base_search_count = None

# Compare projects count: DB truth vs robot projects output
if projects_snap and not has_capture_error(projects_snap):
    # Robot envelope: data may be an array or under a key
    data = projects_snap.get("data", projects_snap)
    if isinstance(data, list):
        surface_n = len(data)
    elif isinstance(data, dict) and "count" in data:
        surface_n = safe_int(data["count"])
    elif isinstance(data, dict) and "projects" in data:
        surface_n = len(data["projects"])
    else:
        surface_n = None
    record_check(build_check(
        surface_id="robot.projects",
        metric="projects.count",
        comparator="eq",
        db_truth=db_projects,
        surface_value=surface_n,
        snapshot="projects_view.json",
        note=fallback_note if fallback_active else None,
    ))

# Compare agents count
if agents_snap and not has_capture_error(agents_snap):
    data = agents_snap.get("data", agents_snap)
    if isinstance(data, list):
        surface_n = len(data)
    elif isinstance(data, dict) and "count" in data:
        surface_n = safe_int(data["count"])
    elif isinstance(data, dict) and "agents" in data:
        surface_n = len(data["agents"])
    else:
        surface_n = None
    record_check(build_check(
        surface_id="robot.agents",
        metric="agents.count_lte_global",
        comparator="lte",
        db_truth=db_agents,
        surface_value=surface_n,
        snapshot="agents_view.json",
        note=fallback_note if fallback_active else None,
    ))

# Compare messages total
if inbox_snap and not has_capture_error(inbox_snap):
    data = inbox_snap.get("data", inbox_snap)
    if isinstance(data, dict) and "count" in data:
        surface_n = safe_int(data["count"])
    elif isinstance(data, dict) and "total" in data:
        surface_n = safe_int(data["total"])
    elif isinstance(data, dict) and "messages" in data:
        surface_n = len(data["messages"])
    elif isinstance(data, dict) and "inbox" in data:
        surface_n = len(data["inbox"])
    elif isinstance(data, list):
        surface_n = len(data)
    else:
        surface_n = None
    base_inbox_count = surface_n
    record_check(build_check(
        surface_id="robot.messages",
        metric="messages.inbox_total_lte_global",
        comparator="lte",
        db_truth=db_messages,
        surface_value=surface_n,
        snapshot="messages_inbox.json",
        note=f"{fallback_note}; surface may be inbox-only (single agent), not global total" if fallback_active else "surface may be inbox-only (single agent), not global total",
    ))

# Compare health endpoint
if health_snap and not has_capture_error(health_snap):
    data = health_snap.get("data", health_snap)
    if isinstance(data, dict):
        probes_n = list_len(data.get("probes"))
        record_check(build_check(
            surface_id="robot.health",
            metric="probes_count_gte_one",
            comparator="gte",
            db_truth=1,
            surface_value=probes_n,
            snapshot="system_health.json",
            note=fallback_note if fallback_active else None,
        ))

# Compare status command (dashboard snapshot)
if dashboard_snap:
    status_note = None
    status_value = None
    status_db = None if fallback_active else min_required(db_threads)
    if has_capture_error(dashboard_snap):
        status_note = "dashboard_status snapshot capture_error"
        status_db = None
    else:
        status_value = list_len(dashboard_snap.get("top_threads"))
        status_note = fallback_note if fallback_active else None
    record_check(build_check(
        surface_id="robot.status",
        metric="top_threads.gte_one_if_any_threads",
        comparator="gte",
        db_truth=status_db,
        surface_value=status_value,
        snapshot="dashboard_status.json",
        note=status_note,
    ))

# Compare thread command
if threads_snap:
    thread_note = None
    thread_value = None
    thread_db = None if fallback_active else min_required(db_threads)
    if has_capture_error(threads_snap):
        thread_note = "threads_view snapshot capture_error"
        thread_db = None
    else:
        thread_value = first_int(threads_snap.get("message_count"), threads_snap.get("count"))
        thread_note = fallback_note if fallback_active else None
    base_thread_count = thread_value
    record_check(build_check(
        surface_id="robot.thread",
        metric="message_count.gte_one_if_any_threads",
        comparator="gte",
        db_truth=thread_db,
        surface_value=thread_value,
        snapshot="threads_view.json",
        note=thread_note,
    ))

# Compare search command
if search_snap:
    search_note = None
    search_value = None
    search_db = None if fallback_active else min_required(db_messages)
    if has_capture_error(search_snap):
        search_note = "search_results snapshot capture_error"
        search_db = None
    else:
        search_value = first_int(search_snap.get("total_results"), search_snap.get("count"))
        search_note = fallback_note if fallback_active else None
    base_search_count = search_value
    record_check(build_check(
        surface_id="robot.search",
        metric="total_results.gte_one_if_any_messages",
        comparator="gte",
        db_truth=search_db,
        surface_value=search_value,
        snapshot="search_results.json",
        note=search_note,
    ))

# Property checks: query-param sweeps should not expand result cardinality
if inbox_limit5_snap:
    limit5_note = None
    limit5_value = None
    if has_capture_error(inbox_limit5_snap):
        limit5_note = "messages_inbox_limit5 snapshot capture_error"
    else:
        data = inbox_limit5_snap.get("data", inbox_limit5_snap)
        if isinstance(data, dict):
            limit5_value = first_int(data.get("count"), data.get("total"))
            if limit5_value is None and isinstance(data.get("inbox"), list):
                limit5_value = len(data["inbox"])
        elif isinstance(data, list):
            limit5_value = len(data)
    record_check(build_check(
        surface_id="robot.messages_params",
        metric="inbox_limit5.lte_baseline",
        comparator="lte",
        db_truth=base_inbox_count,
        surface_value=limit5_value,
        snapshot="messages_inbox_limit5.json",
        note=limit5_note,
    ))
    record_check(build_check(
        surface_id="robot.messages_params",
        metric="inbox_limit5.lte_5",
        comparator="lte",
        db_truth=5,
        surface_value=limit5_value,
        snapshot="messages_inbox_limit5.json",
        note=limit5_note,
    ))

if inbox_unread_snap:
    unread_note = None
    unread_value = None
    if has_capture_error(inbox_unread_snap):
        unread_note = "messages_inbox_unread snapshot capture_error"
    else:
        data = inbox_unread_snap.get("data", inbox_unread_snap)
        if isinstance(data, dict):
            unread_value = first_int(data.get("count"), data.get("total"))
            if unread_value is None and isinstance(data.get("inbox"), list):
                unread_value = len(data["inbox"])
        elif isinstance(data, list):
            unread_value = len(data)
    record_check(build_check(
        surface_id="robot.messages_params",
        metric="inbox_unread.lte_baseline",
        comparator="lte",
        db_truth=base_inbox_count,
        surface_value=unread_value,
        snapshot="messages_inbox_unread.json",
        note=unread_note,
    ))

if threads_limit3_snap:
    thread_limit_note = None
    thread_limit_value = None
    if has_capture_error(threads_limit3_snap):
        thread_limit_note = "threads_view_limit3 snapshot capture_error"
    else:
        thread_limit_value = first_int(
            threads_limit3_snap.get("message_count"),
            threads_limit3_snap.get("count"),
        )
    record_check(build_check(
        surface_id="robot.thread_params",
        metric="thread_limit3.lte_baseline",
        comparator="lte",
        db_truth=base_thread_count,
        surface_value=thread_limit_value,
        snapshot="threads_view_limit3.json",
        note=thread_limit_note,
    ))
    record_check(build_check(
        surface_id="robot.thread_params",
        metric="thread_limit3.lte_3",
        comparator="lte",
        db_truth=3,
        surface_value=thread_limit_value,
        snapshot="threads_view_limit3.json",
        note=thread_limit_note,
    ))

if search_since_snap:
    search_since_note = None
    search_since_value = None
    if has_capture_error(search_since_snap):
        search_since_note = "search_results_since snapshot capture_error"
    else:
        search_since_value = first_int(
            search_since_snap.get("total_results"),
            search_since_snap.get("count"),
        )
    record_check(build_check(
        surface_id="robot.search_params",
        metric="search_since.lte_baseline",
        comparator="lte",
        db_truth=base_search_count,
        surface_value=search_since_value,
        snapshot="search_results_since.json",
        note=search_since_note,
    ))

if search_kind_message_snap:
    search_kind_note = None
    search_kind_value = None
    if has_capture_error(search_kind_message_snap):
        search_kind_note = "search_results_kind_message snapshot capture_error"
    else:
        search_kind_value = first_int(
            search_kind_message_snap.get("total_results"),
            search_kind_message_snap.get("count"),
        )
    record_check(build_check(
        surface_id="robot.search_params",
        metric="search_kind_message.lte_baseline",
        comparator="lte",
        db_truth=base_search_count,
        surface_value=search_kind_value,
        snapshot="search_results_kind_message.json",
        note=search_kind_note,
    ))

# Compare overview command (if captured)
if overview_snap:
    overview_note = None
    overview_value = None
    overview_db = None if fallback_active else min_required(db_projects)
    if has_capture_error(overview_snap):
        overview_note = "overview snapshot capture_error"
        overview_db = None
    else:
        overview_value = first_int(
            overview_snap.get("project_count"),
            overview_snap.get("projects_total"),
            overview_snap.get("count"),
        )
        overview_note = fallback_note if fallback_active else None
    record_check(build_check(
        surface_id="robot.overview",
        metric="project_count.gte_one_if_any_projects",
        comparator="gte",
        db_truth=overview_db,
        surface_value=overview_value,
        snapshot="overview.json",
        note=overview_note,
    ))

# Compare archive/attachments view capture
if attachments_snap:
    attachments_note = None
    attachments_value = None
    attachments_db = 0
    if has_capture_error(attachments_snap):
        attachments_note = "attachments snapshot capture_error"
        attachments_db = None
    else:
        attachments_value = first_int(attachments_snap.get("count"), attachments_snap.get("total"))
        if attachments_value is None and isinstance(attachments_snap.get("attachments"), list):
            attachments_value = len(attachments_snap["attachments"])
        attachments_note = fallback_note if fallback_active else None
    record_check(build_check(
        surface_id="robot.archive",
        metric="attachments.inventory_nonnegative",
        comparator="gte",
        db_truth=attachments_db,
        surface_value=attachments_value,
        snapshot="attachments.json",
        note=attachments_note,
    ))

mismatch_count = len(mismatches)
indeterminate_count = len(indeterminate_checks)
verdict = "PASS" if mismatch_count == 0 and indeterminate_count == 0 else "FAIL"
severity_counts = {"critical": 0, "high": 0, "medium": 0, "low": 0}
for mismatch in mismatches:
    severity = mismatch.get("severity")
    if severity in severity_counts:
        severity_counts[severity] += 1

result = {
    "schema_version": "truth_oracle_report.v1",
    "producer": {"script": "incident_capture_harness.sh", "bead_id": "br-2k3qx.1.3"},
    "mode": "incident_capture",
    "db_truth_global_counts": gc,
    "checks": checks,
    "mismatches": mismatches,
    "indeterminate_checks": indeterminate_checks,
    "summary": {
        "check_count": len(checks),
        "mismatch_count": mismatch_count,
        "indeterminate_count": indeterminate_count,
        "by_severity": severity_counts,
        "verdict": verdict,
    },
    "mismatch_count": mismatch_count,
    "indeterminate_count": indeterminate_count,
    "verdict": verdict,
}

with open(os.path.join(diag_dir, "truth_comparison.json"), "w", encoding="utf-8") as f:
    json.dump(result, f, indent=2, sort_keys=True)
COMPARE

# Read mismatch count back into shell
if [ -f "${OUTPUT_DIR}/diagnostics/truth_comparison.json" ]; then
    TRUTH_MISMATCHES="$(json_get_top_key "${OUTPUT_DIR}/diagnostics/truth_comparison.json" "mismatch_count" "0")"
    TRUTH_INDETERMINATE="$(json_get_top_key "${OUTPUT_DIR}/diagnostics/truth_comparison.json" "indeterminate_count" "0")"
    [ -n "${TRUTH_MISMATCHES}" ] || TRUTH_MISMATCHES=0
    [ -n "${TRUTH_INDETERMINATE}" ] || TRUTH_INDETERMINATE=0
fi

# ---------------------------------------------------------------------------
# Phase 5: Enhanced context.json + repro.sh
# ---------------------------------------------------------------------------
log "writing context.json and repro.sh"

python3 - "${OUTPUT_DIR}" "${DB_PATH}" "${STORAGE_ROOT}" "${PROJECT_KEY}" "${AGENT_NAME}" "${THREAD_ID}" "${MESSAGE_ID}" "${PORT}" "${SEED}" "${FIXTURE_FINGERPRINT}" "${CAPTURE_SUCCESSES}" "${CAPTURE_FAILURES}" "${TRUTH_MISMATCHES}" "${PROJECT_COUNT}" "${AGENT_COUNT}" "${MESSAGE_COUNT}" "${THREAD_COUNT}" "${ROBOT_TIMEOUT_SECS}" <<'CONTEXT'
import json
import os
import sys
from datetime import datetime, timezone

(
    out_dir, db_path, storage_root, project_key, agent_name,
    thread_id, message_id, port, seed, fingerprint,
    cap_ok, cap_fail, mismatches,
    n_proj, n_agent, n_msg, n_thread, robot_timeout,
) = sys.argv[1:19]

context = {
    "bead_id": "br-2k3qx.1.3",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "db_path": db_path,
    "storage_root": storage_root,
    "project_key": project_key,
    "agent_name": agent_name,
    "thread_id": thread_id,
    "message_id": int(message_id),
    "http_port": int(port),
    "seed": int(seed),
    "fixture_fingerprint_sha256": fingerprint,
    "fixture_params": {
        "projects": int(n_proj),
        "agents": int(n_agent),
        "messages": int(n_msg),
        "threads": int(n_thread),
    },
    "capture_stats": {
        "succeeded": int(cap_ok),
        "failed": int(cap_fail),
        "truth_mismatches": int(mismatches),
    },
    "robot_timeout_secs": int(robot_timeout),
}

with open(os.path.join(out_dir, "context.json"), "w", encoding="utf-8") as f:
    json.dump(context, f, indent=2, sort_keys=True)

# Generate repro.sh
repro_script = f"""#!/usr/bin/env bash
# Auto-generated repro script for incident capture
# Bead: br-2k3qx.1.3
# Seed: {seed}  Fingerprint: {fingerprint}
set -euo pipefail
exec scripts/incident_capture_harness.sh \\
    --seed {seed} \\
    --projects {n_proj} \\
    --agents {n_agent} \\
    --messages {n_msg} \\
    --threads {n_thread} \\
    --robot-timeout-secs {robot_timeout} \\
    --verbose \\
    "$@"
"""

repro_path = os.path.join(out_dir, "repro.sh")
with open(repro_path, "w", encoding="utf-8") as f:
    f.write(repro_script)
os.chmod(repro_path, 0o755)
CONTEXT

# ---------------------------------------------------------------------------
# Phase 6: Bundle all artifacts
# ---------------------------------------------------------------------------
log "creating artifact bundle"

(
    cd "${OUTPUT_DIR}"
    tar -czf "incident_capture_bundle.tar.gz" \
        fixture_report.json \
        fixture_seed_stdout.json \
        fixture_seed_stderr.log \
        context.json \
        repro.sh \
        diagnostics \
        logs \
        snapshots
)

# ---------------------------------------------------------------------------
# Phase 7: Enhanced manifest with full snapshot enumeration + verdict
# ---------------------------------------------------------------------------
log "writing manifest"

python3 - "${OUTPUT_DIR}" "${CAPTURE_SUCCESSES}" "${CAPTURE_FAILURES}" "${TRUTH_MISMATCHES}" <<'MANIFEST'
import json
import os
import sys
from datetime import datetime, timezone

out_dir, cap_ok, cap_fail, mismatches = sys.argv[1:5]

# Enumerate all snapshots
snap_dir = os.path.join(out_dir, "snapshots")
snapshots = {}
if os.path.isdir(snap_dir):
    for fname in sorted(os.listdir(snap_dir)):
        key = os.path.splitext(fname)[0]
        snapshots[key] = f"snapshots/{fname}"

manifest = {
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "bead_id": "br-2k3qx.1.3",
    "output_dir": out_dir,
    "files": {
        "fixture_report": "fixture_report.json",
        "context": "context.json",
        "repro_script": "repro.sh",
        "db_counts": "diagnostics/db_counts.tsv",
        "db_truth": "diagnostics/db_truth.json",
        "truth_comparison": "diagnostics/truth_comparison.json",
        "server_log": "logs/server.log",
        "bundle": "incident_capture_bundle.tar.gz",
    },
    "snapshots": snapshots,
    "summary": {
        "captures_succeeded": int(cap_ok),
        "captures_failed": int(cap_fail),
        "truth_mismatches": int(mismatches),
        "verdict": "PASS" if int(mismatches) == 0 and int(cap_fail) == 0 else "FAIL",
    },
}

with open(os.path.join(out_dir, "incident_capture_manifest.json"), "w", encoding="utf-8") as f:
    json.dump(manifest, f, indent=2, sort_keys=True)
MANIFEST

# ---------------------------------------------------------------------------
# Shutdown & final output
# ---------------------------------------------------------------------------
if [ "${KEEP_SERVER}" -eq 1 ] && [ -n "${SERVER_PID}" ]; then
    printf 'Server kept alive: pid=%s url=http://127.0.0.1:%s/mail/\n' "${SERVER_PID}" "${PORT}" >&2
else
    if [ -n "${SERVER_PID}" ]; then
        kill "${SERVER_PID}" >/dev/null 2>&1 || true
        wait "${SERVER_PID}" >/dev/null 2>&1 || true
        SERVER_PID=""
    fi
fi

printf '\n=== Incident Capture Summary ===\n' >&2
printf 'Output:      %s\n' "${OUTPUT_DIR}" >&2
printf 'Captures:    %s succeeded, %s failed\n' "${CAPTURE_SUCCESSES}" "${CAPTURE_FAILURES}" >&2
printf 'Mismatches:  %s\n' "${TRUTH_MISMATCHES}" >&2
printf 'Indeterminate checks: %s\n' "${TRUTH_INDETERMINATE}" >&2
printf 'Fingerprint: %s\n' "${FIXTURE_FINGERPRINT}" >&2
printf 'Manifest:    %s\n' "${OUTPUT_DIR}/incident_capture_manifest.json" >&2
printf 'Bundle:      %s\n' "${OUTPUT_DIR}/incident_capture_bundle.tar.gz" >&2
OK_BOOL=true
if [ "${TRUTH_MISMATCHES}" -gt 0 ]; then
    OK_BOOL=false
    printf 'VERDICT:     FAIL (truth mismatches detected)\n' >&2
elif [ "${TRUTH_INDETERMINATE}" -gt 0 ]; then
    OK_BOOL=false
    printf 'VERDICT:     FAIL (indeterminate truth checks detected)\n' >&2
elif [ "${CAPTURE_FAILURES}" -gt 0 ]; then
    OK_BOOL=false
    printf 'VERDICT:     FAIL (capture failures)\n' >&2
else
    printf 'VERDICT:     PASS\n' >&2
fi
printf '================================\n' >&2

# Structured JSON on stdout
python3 - "${OK_BOOL}" "${OUTPUT_DIR}" "${FIXTURE_FINGERPRINT}" "${CAPTURE_SUCCESSES}" "${CAPTURE_FAILURES}" "${TRUTH_MISMATCHES}" "${TRUTH_INDETERMINATE}" <<'PY'
import json
import sys

ok = sys.argv[1].lower() == "true"
out_dir = sys.argv[2]
fingerprint = sys.argv[3]
captures_succeeded = int(sys.argv[4])
captures_failed = int(sys.argv[5])
truth_mismatches = int(sys.argv[6])
truth_indeterminate = int(sys.argv[7])

print(
    json.dumps(
        {
            "ok": ok,
            "output_dir": out_dir,
            "manifest": f"{out_dir}/incident_capture_manifest.json",
            "bundle": f"{out_dir}/incident_capture_bundle.tar.gz",
            "fixture_fingerprint_sha256": fingerprint,
            "captures_succeeded": captures_succeeded,
            "captures_failed": captures_failed,
            "truth_mismatches": truth_mismatches,
            "truth_indeterminate": truth_indeterminate,
        }
    )
)
PY

# Exit code 2 for non-fatal verification failures (distinct from fatal error exit 1)
if [ "${TRUTH_MISMATCHES}" -gt 0 ] || [ "${TRUTH_INDETERMINATE}" -gt 0 ] || [ "${CAPTURE_FAILURES}" -gt 0 ]; then
    exit 2
fi
