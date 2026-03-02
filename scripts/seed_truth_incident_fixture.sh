#!/usr/bin/env bash
# seed_truth_incident_fixture.sh
#
# Deterministic high-cardinality fixture builder for incident reproduction
# (br-2k3qx.1.1 / Track A1).
#
# This script:
# 1) Initializes a fresh agent-mail SQLite schema via headless server startup
# 2) Bulk-seeds deterministic projects/agents/messages/threads directly in SQLite
# 3) Emits a machine-readable validation report proving fixture integrity
#
# Default scale (matches A1 requirements):
# - projects: 15
# - agents: 300
# - messages: 3200
# - threads: 320

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

DEFAULT_PROJECTS=15
DEFAULT_AGENTS=300
DEFAULT_MESSAGES=3200
DEFAULT_THREADS=320
DEFAULT_SEED=424242

DB_PATH=""
STORAGE_ROOT=""
REPORT_PATH=""
PROJECT_COUNT="${DEFAULT_PROJECTS}"
AGENT_COUNT="${DEFAULT_AGENTS}"
MESSAGE_COUNT="${DEFAULT_MESSAGES}"
THREAD_COUNT="${DEFAULT_THREADS}"
SEED="${DEFAULT_SEED}"
OVERWRITE=0
VERBOSE=0

usage() {
    cat <<'USAGE'
Usage: scripts/seed_truth_incident_fixture.sh [options]

Required:
  --db <path>                SQLite database path
  --storage-root <path>      Storage root used for schema initialization

Optional:
  --report <path>            Validation report path (default: <db>.truth_fixture_report.json)
  --projects <n>             Number of projects (default: 15)
  --agents <n>               Number of agents (default: 300)
  --messages <n>             Number of messages (default: 3200)
  --threads <n>              Number of threads (default: 320)
  --seed <n>                 Deterministic seed (default: 424242)
  --overwrite                Overwrite existing DB file and WAL/SHM sidecars
  --verbose                  Print extra progress details
  -h, --help                 Show help

Example:
  scripts/seed_truth_incident_fixture.sh \
    --db /tmp/truth_fixture.sqlite3 \
    --storage-root /tmp/truth_fixture_storage \
    --seed 20260302 \
    --report /tmp/truth_fixture_report.json
USAGE
}

log() {
    if [ "${VERBOSE}" -eq 1 ]; then
        printf '[seed-fixture] %s\n' "$*" >&2
    fi
}

die() {
    printf 'ERROR: %s\n' "$*" >&2
    exit 1
}

require_cmd() {
    local cmd="$1"
    command -v "$cmd" >/dev/null 2>&1 || die "missing required command: ${cmd}"
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
    local attempts="${2:-100}"
    local delay="${3:-0.1}"
    local url="http://127.0.0.1:${port}/health"
    local i
    for i in $(seq 1 "${attempts}"); do
        if curl -sS -o /dev/null --connect-timeout 1 "${url}" 2>/dev/null; then
            return 0
        fi
        sleep "${delay}"
    done
    return 1
}

initialize_schema() {
    local db_path="$1"
    local storage_root="$2"
    local port
    local pid
    local log_path

    port="$(pick_port)"
    log_path="$(mktemp "${TMPDIR:-/tmp}/seed_fixture_server.XXXXXX.log")"
    log "initializing schema using mcp-agent-mail serve on port ${port}"

    DATABASE_URL="sqlite:///${db_path}" \
    STORAGE_ROOT="${storage_root}" \
    HTTP_HOST="127.0.0.1" \
    HTTP_PORT="${port}" \
    TUI_ENABLED=false \
    mcp-agent-mail serve --no-tui >"${log_path}" 2>&1 &
    pid=$!

    if ! wait_for_health "${port}" 150 0.1; then
        kill "${pid}" >/dev/null 2>&1 || true
        wait "${pid}" >/dev/null 2>&1 || true
        sed -n '1,200p' "${log_path}" >&2 || true
        rm -f "${log_path}" >/dev/null 2>&1 || true
        die "failed to initialize schema (server did not become healthy)"
    fi

    kill "${pid}" >/dev/null 2>&1 || true
    wait "${pid}" >/dev/null 2>&1 || true
    rm -f "${log_path}" >/dev/null 2>&1 || true
}

while [ $# -gt 0 ]; do
    case "$1" in
        --db)
            DB_PATH="${2:-}"
            shift 2
            ;;
        --storage-root)
            STORAGE_ROOT="${2:-}"
            shift 2
            ;;
        --report)
            REPORT_PATH="${2:-}"
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
        --seed)
            SEED="${2:-}"
            shift 2
            ;;
        --overwrite)
            OVERWRITE=1
            shift 1
            ;;
        --verbose)
            VERBOSE=1
            shift 1
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

[ -n "${DB_PATH}" ] || die "--db is required"
[ -n "${STORAGE_ROOT}" ] || die "--storage-root is required"
[ -n "${PROJECT_COUNT}" ] || die "--projects cannot be empty"
[ -n "${AGENT_COUNT}" ] || die "--agents cannot be empty"
[ -n "${MESSAGE_COUNT}" ] || die "--messages cannot be empty"
[ -n "${THREAD_COUNT}" ] || die "--threads cannot be empty"
[ -n "${SEED}" ] || die "--seed cannot be empty"

case "${PROJECT_COUNT}" in (*[!0-9]*|"") die "--projects must be an integer >= 1" ;; esac
case "${AGENT_COUNT}" in (*[!0-9]*|"") die "--agents must be an integer >= 1" ;; esac
case "${MESSAGE_COUNT}" in (*[!0-9]*|"") die "--messages must be an integer >= 1" ;; esac
case "${THREAD_COUNT}" in (*[!0-9]*|"") die "--threads must be an integer >= 1" ;; esac
case "${SEED}" in (*[!0-9]*|"") die "--seed must be an integer >= 0" ;; esac

[ "${PROJECT_COUNT}" -ge 1 ] || die "--projects must be >= 1"
[ "${AGENT_COUNT}" -ge 1 ] || die "--agents must be >= 1"
[ "${MESSAGE_COUNT}" -ge 1 ] || die "--messages must be >= 1"
[ "${THREAD_COUNT}" -ge 1 ] || die "--threads must be >= 1"
[ "${SEED}" -ge 0 ] || die "--seed must be >= 0"
[ "${MESSAGE_COUNT}" -ge "${THREAD_COUNT}" ] || die "--messages must be >= --threads"

require_cmd mcp-agent-mail
require_cmd python3
require_cmd sqlite3
require_cmd curl

mkdir -p "$(dirname "${DB_PATH}")" "${STORAGE_ROOT}"

if [ -f "${DB_PATH}" ] || [ -f "${DB_PATH}-wal" ] || [ -f "${DB_PATH}-shm" ]; then
    if [ "${OVERWRITE}" -ne 1 ]; then
        die "database already exists at ${DB_PATH} (use --overwrite)"
    fi
    rm -f "${DB_PATH}" "${DB_PATH}-wal" "${DB_PATH}-shm"
fi

touch "${DB_PATH}"

if [ -z "${REPORT_PATH}" ]; then
    REPORT_PATH="${DB_PATH}.truth_fixture_report.json"
fi
mkdir -p "$(dirname "${REPORT_PATH}")"

initialize_schema "${DB_PATH}" "${STORAGE_ROOT}"

log "schema initialized at ${DB_PATH}"

python3 - "${DB_PATH}" "${REPORT_PATH}" "${SEED}" "${PROJECT_COUNT}" "${AGENT_COUNT}" "${MESSAGE_COUNT}" "${THREAD_COUNT}" <<'PY'
import hashlib
import json
import random
import sqlite3
import sys
from datetime import datetime, timezone

db_path, report_path, seed_raw, projects_raw, agents_raw, messages_raw, threads_raw = sys.argv[1:8]
seed = int(seed_raw)
project_count = int(projects_raw)
agent_count = int(agents_raw)
message_count = int(messages_raw)
thread_count = int(threads_raw)

base_ts = 1_706_400_000_000_000 + (seed % 50_000_000)

programs = ["claude-code", "codex-cli", "gemini-cli", "cursor"]
models = ["opus-4.6", "gpt-5-codex", "gemini-2.5-pro", "sonnet-4.6"]
adjectives = [
    "Amber", "Blue", "Crimson", "Cobalt", "Golden", "Gray", "Ivory", "Jade",
    "Lime", "Mint", "Navy", "Olive", "Pearl", "Plum", "Red", "Silver", "Slate",
    "Teal", "Umber", "Vivid",
]
nouns = [
    "Aster", "Bison", "Cedar", "Comet", "Dune", "Falcon", "Flint", "Grove",
    "Harbor", "Juniper", "Lynx", "Mesa", "Orchid", "Pine", "Quartz", "Ridge",
    "Spruce", "Thorn", "Vale", "Willow",
]

markdown_templates = [
    (
        "# Incident Snapshot {idx}\n\n"
        "## Scope\n"
        "- project: `{project_slug}`\n"
        "- thread: `{thread_id}`\n"
        "- sender: `{sender}`\n"
    ),
    (
        "### Checklist {idx}\n\n"
        "- [x] baseline captured\n"
        "- [ ] reproduce mismatch in `{thread_id}`\n"
        "- [ ] verify fix in dashboard/messages/threads\n"
    ),
    (
        "```rust\n"
        "fn verify_truth(expected: i64, observed: i64) -> bool {{\n"
        "    expected == observed\n"
        "}}\n"
        "```\n\n"
        "Runbook item `{idx}` for `{thread_id}`."
    ),
    (
        "| field | expected | observed |\n"
        "| --- | --- | --- |\n"
        "| projects | >=15 | {projects} |\n"
        "| agents | >=300 | {agents} |\n"
        "| messages | >=3000 | {messages} |\n"
        "| threads | >=300 | {threads} |\n"
    ),
    (
        "> Escalation note {idx}: operators need truthful, rendered data.\n\n"
        "Reference: https://example.com/incident/{idx}\n"
    ),
    (
        "Long-form context {idx}.\n\n"
        "This fixture intentionally includes multi-line markdown bodies to ensure\n"
        "renderers and adapters do not collapse content into placeholders.\n\n"
        "Paragraph 2 for `{thread_id}` with deterministic seed `{seed}`."
    ),
    (
        "1. Gather DB truth for `{project_slug}`\n"
        "2. Compare against UI rows for `{thread_id}`\n"
        "3. Record mismatch evidence `{idx}`\n\n"
        "Inline code: `SELECT COUNT(*) FROM messages`."
    ),
    (
        "Plain text fallback body {idx} for `{thread_id}`.\n"
        "No markdown heading here; verifies fallback rendering path."
    ),
]

validation_queries = {
    "projects": "SELECT COUNT(*) FROM projects",
    "agents": "SELECT COUNT(*) FROM agents",
    "messages": "SELECT COUNT(*) FROM messages",
    "threads": "SELECT COUNT(DISTINCT thread_id) FROM messages WHERE thread_id IS NOT NULL AND thread_id != ''",
    "ack_required_messages": "SELECT COUNT(*) FROM messages WHERE ack_required = 1",
    "recipient_rows": "SELECT COUNT(*) FROM message_recipients",
    "recipient_reads": "SELECT COUNT(*) FROM message_recipients WHERE read_ts IS NOT NULL",
    "recipient_acks": "SELECT COUNT(*) FROM message_recipients WHERE ack_ts IS NOT NULL",
    "markdown_heading_rows": "SELECT COUNT(*) FROM messages WHERE body_md LIKE '# %' OR body_md LIKE '### %'",
    "markdown_code_rows": "SELECT COUNT(*) FROM messages WHERE body_md LIKE '%```rust%'",
    "markdown_table_rows": "SELECT COUNT(*) FROM messages WHERE body_md LIKE '%| --- |%'",
    "markdown_link_rows": "SELECT COUNT(*) FROM messages WHERE body_md LIKE '%https://example.com/%'",
    "markdown_quote_rows": "SELECT COUNT(*) FROM messages WHERE body_md LIKE '> %'",
    "markdown_task_rows": "SELECT COUNT(*) FROM messages WHERE body_md LIKE '%- [ ]%'",
    "max_body_len": "SELECT MAX(LENGTH(body_md)) FROM messages",
}


def split_even(total: int, buckets: int) -> list[int]:
    base = total // buckets
    rem = total % buckets
    return [base + (1 if i < rem else 0) for i in range(buckets)]


conn = sqlite3.connect(db_path)
conn.execute("PRAGMA journal_mode=WAL")
conn.execute("PRAGMA synchronous=NORMAL")
cur = conn.cursor()

# Reset core fixture tables for deterministic reruns on an initialized schema.
cur.execute("PRAGMA foreign_keys=OFF")
cur.execute("DELETE FROM message_recipients")
cur.execute("DELETE FROM messages")
cur.execute("DELETE FROM agents")
cur.execute("DELETE FROM projects")
cur.execute("SELECT 1 FROM sqlite_master WHERE type='table' AND name='sqlite_sequence'")
if cur.fetchone() is not None:
    cur.execute("DELETE FROM sqlite_sequence WHERE name IN ('projects','agents','messages')")
cur.execute("PRAGMA foreign_keys=ON")
conn.commit()

# Projects
project_ids: list[int] = []
project_slugs: list[str] = []
for p_idx in range(project_count):
    slug = f"truth-proj-{p_idx:03d}"
    human_key = f"/data/e2e/truth-fixture/project-{p_idx:03d}"
    created_at = base_ts + (p_idx * 10_000)
    cur.execute(
        "INSERT INTO projects (slug, human_key, created_at) VALUES (?, ?, ?)",
        (slug, human_key, created_at),
    )
    project_ids.append(int(cur.lastrowid))
    project_slugs.append(slug)
project_slug_by_id = dict(zip(project_ids, project_slugs))

# Agents
agents_by_project: dict[int, list[tuple[int, str]]] = {pid: [] for pid in project_ids}
agents_per_project = split_even(agent_count, project_count)
agent_global = 0
for p_idx, pid in enumerate(project_ids):
    for local_idx in range(agents_per_project[p_idx]):
        adjective = adjectives[(agent_global + seed) % len(adjectives)]
        noun = nouns[(agent_global * 3 + seed) % len(nouns)]
        name = f"{adjective}{noun}{agent_global:03d}"
        program = programs[agent_global % len(programs)]
        model = models[agent_global % len(models)]
        ts = base_ts + 1_000_000 + (agent_global * 1_000)
        cur.execute(
            "INSERT INTO agents (project_id, name, program, model, task_description, inception_ts, last_active_ts, attachments_policy, contact_policy) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, 'auto', 'auto')",
            (pid, name, program, model, "truth fixture seed", ts, ts),
        )
        agents_by_project[pid].append((int(cur.lastrowid), name))
        agent_global += 1

# Thread plan
threads_per_project = split_even(thread_count, project_count)
thread_plan: list[tuple[int, str]] = []
for p_idx, pid in enumerate(project_ids):
    for t_idx in range(threads_per_project[p_idx]):
        thread_plan.append((pid, f"TRUTH-P{p_idx:03d}-T{t_idx:04d}"))

assert len(thread_plan) == thread_count, "thread plan mismatch"

messages_per_thread = split_even(message_count, thread_count)

# Messages + recipients
message_global = 0
importance_cycle = ["normal", "high", "urgent", "low"]
for thread_idx, (pid, thread_id) in enumerate(thread_plan):
    project_slug = project_slug_by_id[pid]
    project_agents = agents_by_project[pid]
    msg_for_thread = messages_per_thread[thread_idx]
    for msg_local in range(msg_for_thread):
        sender_slot = (message_global + msg_local + thread_idx) % len(project_agents)
        recipient_slot = (sender_slot + 1) % len(project_agents)
        sender_id, sender_name = project_agents[sender_slot]
        recipient_id, _recipient_name = project_agents[recipient_slot]
        template = markdown_templates[message_global % len(markdown_templates)]
        body = template.format(
            idx=message_global,
            project_slug=project_slug,
            thread_id=thread_id,
            sender=sender_name,
            projects=project_count,
            agents=agent_count,
            messages=message_count,
            threads=thread_count,
            seed=seed,
        )
        subject = f"[{thread_id}] truth-fixture message {msg_local + 1}/{msg_for_thread}"
        importance = importance_cycle[message_global % len(importance_cycle)]
        ack_required = 1 if message_global % 6 == 0 else 0
        created_ts = base_ts + 10_000_000 + (message_global * 10_000)
        cur.execute(
            "INSERT INTO messages (project_id, sender_id, thread_id, subject, body_md, importance, ack_required, created_ts, attachments) "
            "VALUES (?, ?, ?, ?, ?, ?, ?, ?, '[]')",
            (pid, sender_id, thread_id, subject, body, importance, ack_required, created_ts),
        )
        message_id = int(cur.lastrowid)

        read_ts = created_ts + 2_000 if (message_global % 3) != 0 else None
        ack_ts = None
        if ack_required and (message_global % 12 == 0):
            if read_ts is None:
                read_ts = created_ts + 2_500
            ack_ts = created_ts + 3_000

        kind = "to" if (message_global % 11) else "cc"
        cur.execute(
            "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (?, ?, ?, ?, ?)",
            (message_id, recipient_id, kind, read_ts, ack_ts),
        )

        message_global += 1

conn.commit()

# Validation report
validation_results = {}
for key, sql in validation_queries.items():
    cur.execute(sql)
    value = cur.fetchone()[0]
    if value is None:
        value = 0
    validation_results[key] = int(value)

summary = {
    "seed": seed,
    "requested": {
        "projects": project_count,
        "agents": agent_count,
        "messages": message_count,
        "threads": thread_count,
    },
    "counts": {
        "projects": validation_results["projects"],
        "agents": validation_results["agents"],
        "messages": validation_results["messages"],
        "threads": validation_results["threads"],
    },
    "state_mix": {
        "ack_required_messages": validation_results["ack_required_messages"],
        "recipient_rows": validation_results["recipient_rows"],
        "recipient_reads": validation_results["recipient_reads"],
        "recipient_acks": validation_results["recipient_acks"],
    },
    "markdown_coverage": {
        "heading_rows": validation_results["markdown_heading_rows"],
        "code_rows": validation_results["markdown_code_rows"],
        "table_rows": validation_results["markdown_table_rows"],
        "link_rows": validation_results["markdown_link_rows"],
        "quote_rows": validation_results["markdown_quote_rows"],
        "task_rows": validation_results["markdown_task_rows"],
        "max_body_len": validation_results["max_body_len"],
    },
    "validation_queries": validation_queries,
}

fingerprint_material = {
    "seed": seed,
    "counts": summary["counts"],
    "state_mix": summary["state_mix"],
    "markdown_coverage": summary["markdown_coverage"],
}
fingerprint_json = json.dumps(fingerprint_material, sort_keys=True, separators=(",", ":"))
summary["dataset_fingerprint_sha256"] = hashlib.sha256(fingerprint_json.encode("utf-8")).hexdigest()
summary["generated_at"] = datetime.now(timezone.utc).isoformat()
summary["db_path"] = db_path

failures = []
if summary["counts"]["projects"] < 15:
    failures.append("projects < 15")
if summary["counts"]["agents"] < 300:
    failures.append("agents < 300")
if summary["counts"]["messages"] < 3000:
    failures.append("messages < 3000")
if summary["counts"]["threads"] < 300:
    failures.append("threads < 300")

for key in ("heading_rows", "code_rows", "table_rows", "link_rows", "quote_rows", "task_rows"):
    if summary["markdown_coverage"][key] <= 0:
        failures.append(f"markdown coverage missing for {key}")

if summary["state_mix"]["recipient_reads"] <= 0:
    failures.append("recipient_reads == 0")
if summary["state_mix"]["recipient_acks"] <= 0:
    failures.append("recipient_acks == 0")

summary["ok"] = len(failures) == 0
summary["failures"] = failures

with open(report_path, "w", encoding="utf-8") as f:
    json.dump(summary, f, indent=2, sort_keys=True)

print(json.dumps(summary, sort_keys=True))

if failures:
    raise SystemExit(
        "fixture validation failed: " + "; ".join(failures)
    )
PY

printf 'Fixture seeded successfully.\n' >&2
printf 'DB: %s\n' "${DB_PATH}" >&2
printf 'Report: %s\n' "${REPORT_PATH}" >&2
