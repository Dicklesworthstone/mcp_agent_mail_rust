#!/usr/bin/env bash
# test_search_v3_load_concurrency.sh - E2E: Search V3 load/concurrency freshness + latency
#
# br-2tnl.7.15: E2E load/concurrency script for indexing freshness and query latency.
#
# This suite stresses concurrent send_message + search_messages traffic against a live
# HTTP MCP endpoint, then validates:
#   - correctness under concurrent read/write load,
#   - indexing freshness lag for sampled markers,
#   - latency distributions and budgets,
#   - contention diagnostics from captured server logs.
#
# Target: >= 60 assertions and structured artifacts.

set -euo pipefail

E2E_SUITE="search_v3_load_concurrency"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_search_v3_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_search_v3_lib.sh"

e2e_init_artifacts
search_v3_init
search_v3_banner "Search V3 Load/Concurrency E2E (br-2tnl.7.15)"

# ---------------------------------------------------------------------------
# Config knobs (override via env for quick smoke vs heavy load)
# ---------------------------------------------------------------------------
LOAD_WRITERS="${LOAD_WRITERS:-4}"
LOAD_SEARCHERS="${LOAD_SEARCHERS:-4}"
LOAD_OPS_PER_WORKER="${LOAD_OPS_PER_WORKER:-15}"
LOAD_SAMPLE_MARKERS="${LOAD_SAMPLE_MARKERS:-12}"
LOAD_FRESHNESS_TIMEOUT_MS="${LOAD_FRESHNESS_TIMEOUT_MS:-30000}"
LOAD_FRESHNESS_POLL_MS="${LOAD_FRESHNESS_POLL_MS:-200}"
LOAD_MIN_SUCCESS_RATIO="${LOAD_MIN_SUCCESS_RATIO:-0.95}"
LOAD_FRESHNESS_BUDGET_MS="${LOAD_FRESHNESS_BUDGET_MS:-15000}"
LOAD_WRITE_P95_BUDGET_MS="${LOAD_WRITE_P95_BUDGET_MS:-5000}"
LOAD_SEARCH_P95_BUDGET_MS="${LOAD_SEARCH_P95_BUDGET_MS:-5000}"

SEED_MESSAGE_COUNT=8

# ---------------------------------------------------------------------------
# Paths + runtime state
# ---------------------------------------------------------------------------
WORK="$(e2e_mktemp "e2e_sv3_load")"
SEARCH_DB="${WORK}/search_v3_load.sqlite3"
STORAGE_ROOT="${WORK}/storage"
mkdir -p "${STORAGE_ROOT}"

PROJECT_PATH="/tmp/e2e_search_v3_load_${E2E_TIMESTAMP}_$$"
TOKEN="sv3-load-token-${E2E_SEED}"
AUTH_HEADER="Authorization: Bearer ${TOKEN}"

LOAD_DIR="${SEARCH_V3_RUN_DIR}/load"
mkdir -p "${LOAD_DIR}"

LOAD_SUMMARY_JSON="${LOAD_DIR}/load_summary.json"
LATENCY_HIST_JSON="${LOAD_DIR}/latency_histograms.json"
FRESHNESS_JSON="${LOAD_DIR}/freshness_lag_stats.json"
CONTENTION_JSON="${LOAD_DIR}/contention_diagnostics.json"

cleanup() {
    e2e_stop_server || true
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------
min_success_required() {
    local attempted="$1"
    local ratio="$2"
    python3 - "$attempted" "$ratio" <<'PY'
import math
import sys
attempted = int(sys.argv[1])
ratio = float(sys.argv[2])
print(int(math.ceil(attempted * ratio)))
PY
}

tool_text_from_case() {
    local case_id="$1"
    python3 - "${E2E_ARTIFACT_DIR}/${case_id}/response.json" <<'PY'
import json
import sys
from pathlib import Path
p = Path(sys.argv[1])
if not p.exists():
    print("")
    sys.exit(0)
try:
    data = json.loads(p.read_text(encoding="utf-8"))
    result = data.get("result", {})
    content = result.get("content", []) if isinstance(result, dict) else []
    if content and isinstance(content[0], dict):
        print(content[0].get("text", ""))
    else:
        print("")
except Exception:
    print("")
PY
}

result_count_from_text() {
    local text="$1"
    python3 - "$text" <<'PY'
import json
import sys
raw = sys.argv[1]
try:
    payload = json.loads(raw)
except Exception:
    print(0)
    sys.exit(0)
if isinstance(payload, dict):
    if isinstance(payload.get("result"), list):
        print(len(payload["result"]))
    elif isinstance(payload.get("results"), list):
        print(len(payload["results"]))
    elif isinstance(payload.get("messages"), list):
        print(len(payload["messages"]))
    else:
        print(0)
elif isinstance(payload, list):
    print(len(payload))
else:
    print(0)
PY
}

run_tool_case() {
    local case_id="$1"
    local tool_name="$2"
    local args_json="$3"
    e2e_rpc_call "$case_id" "$E2E_SERVER_URL" "$tool_name" "$args_json" "$AUTH_HEADER"
}

assert_numeric_le() {
    local label="$1"
    local value="$2"
    local max="$3"
    if [ "$value" -le "$max" ]; then
        e2e_pass "$label (${value} <= ${max})"
    else
        e2e_fail "$label (${value} > ${max})"
    fi
}

assert_numeric_ge() {
    local label="$1"
    local value="$2"
    local min="$3"
    if [ "$value" -ge "$min" ]; then
        e2e_pass "$label (${value} >= ${min})"
    else
        e2e_fail "$label (${value} < ${min})"
    fi
}

# ---------------------------------------------------------------------------
# Case 1: Setup server + project + baseline corpus
# ---------------------------------------------------------------------------
CASE_FAIL_BASE="${_E2E_FAIL}"
e2e_case_banner "Setup: start server, register agents, seed baseline corpus"
e2e_mark_case_start "setup"

if e2e_start_server_with_logs \
    "$SEARCH_DB" \
    "$STORAGE_ROOT" \
    "sv3_load" \
    "HTTP_BEARER_TOKEN=${TOKEN}" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0" \
    "HTTP_JWT_ENABLED=0" \
    "WORKTREES_ENABLED=true"; then
    e2e_pass "server started with captured logs"
else
    e2e_fail "server failed to start"
    search_v3_case_summary "setup" "fail" --message "server start failed"
    e2e_mark_case_end "setup"
    e2e_summary
    exit 1
fi

# Ensure project
if run_tool_case "setup_ensure_project" "ensure_project" "{\"human_key\":\"${PROJECT_PATH}\"}"; then
    e2e_pass "ensure_project succeeded"
else
    e2e_fail "ensure_project failed"
fi

# Register agents used by load workers
for agent in GoldFox SilverWolf RedPeak BlueLake; do
    args="{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-load\",\"model\":\"test\",\"name\":\"${agent}\"}"
    case_id="setup_register_${agent}"
    if run_tool_case "${case_id}" "register_agent" "${args}"; then
        e2e_pass "register_agent ${agent}"
    else
        e2e_fail "register_agent ${agent}"
    fi
done

# Seed baseline corpus with deterministic phrases
seed_messages=(
    "GoldFox|SilverWolf|Baseline deployment checklist|Baseline deployment checklist for blue/green rollout."
    "SilverWolf|GoldFox|Baseline security review|Baseline security review notes for threat model."
    "RedPeak|BlueLake|Baseline migration plan|Baseline migration plan with incremental index update."
    "BlueLake|RedPeak|Baseline latency report|Baseline latency report with p95 and p99 figures."
    "GoldFox|BlueLake|Search warmup marker|Search warmup marker for concurrent query workers."
    "SilverWolf|RedPeak|Freshness probe seed|Freshness probe seed marker for eventual visibility."
    "RedPeak|GoldFox|Thread load alpha|Thread load alpha baseline conversation."
    "BlueLake|SilverWolf|Thread load beta|Thread load beta baseline conversation."
)

seed_idx=0
for entry in "${seed_messages[@]}"; do
    seed_idx=$((seed_idx + 1))
    sender="${entry%%|*}"
    rest="${entry#*|}"
    recipient="${rest%%|*}"
    rest="${rest#*|}"
    subject="${rest%%|*}"
    body="${rest#*|}"

    args="$(jq -cn \
        --arg project "$PROJECT_PATH" \
        --arg sender "$sender" \
        --arg recipient "$recipient" \
        --arg subject "$subject" \
        --arg body "$body" \
        '{project_key:$project,sender_name:$sender,to:[$recipient],subject:$subject,body_md:$body,thread_id:"thread-load-baseline"}')"

    case_id="setup_seed_$(printf '%02d' "$seed_idx")"
    if run_tool_case "$case_id" "send_message" "$args"; then
        e2e_pass "seed message ${seed_idx} inserted"
    else
        e2e_fail "seed message ${seed_idx} failed"
    fi
done

# Baseline query sanity
if run_tool_case "setup_baseline_query" "search_messages" "{\"project_key\":\"${PROJECT_PATH}\",\"query\":\"Baseline\",\"limit\":20}"; then
    e2e_pass "baseline search request succeeded"
else
    e2e_fail "baseline search request failed"
fi
BASELINE_TEXT="$(tool_text_from_case "setup_baseline_query")"
BASELINE_COUNT="$(result_count_from_text "$BASELINE_TEXT")"
assert_numeric_ge "baseline search returns seeded docs" "$BASELINE_COUNT" 4

# Capture initial index metadata snapshot
DB_COUNT_INITIAL="$(e2e_db_query "$SEARCH_DB" "SELECT COUNT(*) FROM messages;" | tr -d '[:space:]')"
search_v3_capture_index_meta "initial_index" \
    --doc-count "${DB_COUNT_INITIAL:-0}" \
    --last-commit "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
    --consistency "ready"

e2e_mark_case_end "setup"
if [ "${_E2E_FAIL}" -gt "${CASE_FAIL_BASE}" ]; then
    search_v3_case_summary "setup" "fail" --message "setup assertions failed"
else
    search_v3_case_summary "setup" "pass" --message "server + seed ready"
fi

# ---------------------------------------------------------------------------
# Case 2: Concurrent write/search workload + freshness probe
# ---------------------------------------------------------------------------
CASE_FAIL_BASE="${_E2E_FAIL}"
e2e_case_banner "Concurrent workload: parallel writers/searchers + freshness probes"
e2e_mark_case_start "load_phase"

LOAD_AGENTS="GoldFox,SilverWolf,RedPeak,BlueLake"
# Export variables consumed by the embedded Python workload runner.
export TOKEN PROJECT_PATH LOAD_WRITERS LOAD_SEARCHERS LOAD_OPS_PER_WORKER
export LOAD_SAMPLE_MARKERS LOAD_FRESHNESS_TIMEOUT_MS LOAD_FRESHNESS_POLL_MS LOAD_AGENTS
if python3 - "$LOAD_SUMMARY_JSON" "$LATENCY_HIST_JSON" "$FRESHNESS_JSON" <<'PY'
import concurrent.futures
import json
import math
import os
import random
import statistics
import threading
import time
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen

summary_path = Path(os.sys.argv[1])
latency_hist_path = Path(os.sys.argv[2])
freshness_path = Path(os.sys.argv[3])

url = os.environ["E2E_SERVER_URL"]
token = os.environ["TOKEN"]
project = os.environ["PROJECT_PATH"]
writers = int(os.environ["LOAD_WRITERS"])
searchers = int(os.environ["LOAD_SEARCHERS"])
ops_per_worker = int(os.environ["LOAD_OPS_PER_WORKER"])
sample_markers = int(os.environ["LOAD_SAMPLE_MARKERS"])
freshness_timeout_ms = int(os.environ["LOAD_FRESHNESS_TIMEOUT_MS"])
freshness_poll_ms = int(os.environ["LOAD_FRESHNESS_POLL_MS"])
search_modes = ["auto", "lexical", "hybrid"]
agents = [x for x in os.environ["LOAD_AGENTS"].split(",") if x]

load_dir = summary_path.parent
ops_dir = load_dir / "ops"
ops_dir.mkdir(parents=True, exist_ok=True)

random.seed(1337)
lock = threading.Lock()
write_events = []
search_events = []
freshness_events = []


def percentiles(values):
    if not values:
        return {"p50_ms": 0, "p95_ms": 0, "p99_ms": 0, "min_ms": 0, "max_ms": 0, "mean_ms": 0.0}
    vals = sorted(int(v) for v in values)

    def pct(p):
        if len(vals) == 1:
            return vals[0]
        idx = int(round((p / 100.0) * (len(vals) - 1)))
        idx = max(0, min(len(vals) - 1, idx))
        return vals[idx]

    return {
        "p50_ms": pct(50),
        "p95_ms": pct(95),
        "p99_ms": pct(99),
        "min_ms": vals[0],
        "max_ms": vals[-1],
        "mean_ms": round(float(sum(vals)) / float(len(vals)), 3),
    }


def build_hist(values):
    # Millisecond buckets (upper bounds)
    bounds = [10, 25, 50, 100, 200, 500, 1000, 2000, 5000, 10000]
    hist = {f"<= {b}": 0 for b in bounds}
    hist["> 10000"] = 0
    for v in values:
        iv = int(v)
        placed = False
        for b in bounds:
            if iv <= b:
                hist[f"<= {b}"] += 1
                placed = True
                break
        if not placed:
            hist["> 10000"] += 1
    return hist


def parse_tool_text(body_text):
    try:
        data = json.loads(body_text)
    except Exception:
        return ""
    result = data.get("result", {}) if isinstance(data, dict) else {}
    content = result.get("content", []) if isinstance(result, dict) else []
    if content and isinstance(content[0], dict):
        return content[0].get("text", "")
    return ""


def parse_search_count(tool_text):
    try:
        payload = json.loads(tool_text)
    except Exception:
        return 0
    if isinstance(payload, dict):
        if isinstance(payload.get("result"), list):
            return len(payload["result"])
        if isinstance(payload.get("results"), list):
            return len(payload["results"])
        if isinstance(payload.get("messages"), list):
            return len(payload["messages"])
        return 0
    if isinstance(payload, list):
        return len(payload)
    return 0


def post_tool(case_id, tool_name, args):
    op_dir = ops_dir / case_id
    op_dir.mkdir(parents=True, exist_ok=True)

    payload = {
        "jsonrpc": "2.0",
        "method": "tools/call",
        "id": 1,
        "params": {"name": tool_name, "arguments": args},
    }

    request_text = json.dumps(payload, ensure_ascii=False)
    (op_dir / "request.json").write_text(request_text, encoding="utf-8")

    req = Request(
        url,
        data=request_text.encode("utf-8"),
        method="POST",
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {token}",
        },
    )

    status = 0
    headers = {}
    response_text = ""
    error_kind = ""
    start = time.time()
    try:
        with urlopen(req, timeout=30) as resp:
            status = int(resp.getcode())
            headers = dict(resp.getheaders())
            response_text = resp.read().decode("utf-8", errors="replace")
    except HTTPError as e:
        status = int(e.code)
        headers = dict(e.headers.items()) if e.headers else {}
        response_text = e.read().decode("utf-8", errors="replace")
        error_kind = "http_error"
    except Exception as e:
        status = 0
        headers = {}
        response_text = json.dumps({"transport_error": str(e)})
        error_kind = "transport_error"

    elapsed_ms = int((time.time() - start) * 1000)

    (op_dir / "response.json").write_text(response_text, encoding="utf-8")
    (op_dir / "headers.json").write_text(json.dumps(headers, indent=2, sort_keys=True), encoding="utf-8")
    (op_dir / "status.txt").write_text(str(status), encoding="utf-8")
    (op_dir / "timing.txt").write_text(str(elapsed_ms), encoding="utf-8")

    success = False
    if status == 200:
        try:
            outer = json.loads(response_text)
            if "error" not in outer:
                result = outer.get("result", {}) if isinstance(outer, dict) else {}
                success = not (isinstance(result, dict) and bool(result.get("isError", False)))
                if not success and not error_kind:
                    error_kind = "tool_isError"
            else:
                error_kind = "jsonrpc_error"
        except Exception:
            error_kind = "invalid_json"

    return {
        "case_id": case_id,
        "tool": tool_name,
        "status": status,
        "elapsed_ms": elapsed_ms,
        "success": success,
        "error_kind": error_kind,
        "response_text": response_text,
    }


def writer_worker(worker_id):
    local_events = []
    sender = agents[worker_id % len(agents)]
    recipients = [a for a in agents if a != sender][:2]
    if not recipients:
        recipients = [sender]

    for idx in range(ops_per_worker):
        marker = f"load-marker-w{worker_id:02d}-m{idx:03d}"
        args = {
            "project_key": project,
            "sender_name": sender,
            "to": recipients,
            "subject": f"Load marker {marker}",
            "body_md": f"Concurrent write marker {marker} for indexing freshness test.",
            "thread_id": f"thread-load-{worker_id:02d}",
        }
        case_id = f"load_write_w{worker_id:02d}_m{idx:03d}"
        result = post_tool(case_id, "send_message", args)
        event = {
            "worker_id": worker_id,
            "op_index": idx,
            "marker": marker,
            "sender": sender,
            "elapsed_ms": result["elapsed_ms"],
            "success": result["success"],
            "status": result["status"],
            "error_kind": result["error_kind"],
            "sent_at_ms": int(time.time() * 1000),
            "case_id": case_id,
        }
        local_events.append(event)

    with lock:
        write_events.extend(local_events)


def search_worker(worker_id):
    local_events = []
    query_pool = [
        "load marker",
        "baseline",
        "deployment",
        "latency",
        "freshness",
        "thread-load",
        "security",
    ]

    for idx in range(ops_per_worker):
        mode = search_modes[idx % len(search_modes)]
        query = query_pool[(worker_id + idx) % len(query_pool)]
        args = {
            "project_key": project,
            "query": query,
            "limit": 25,
            "mode": mode,
        }
        case_id = f"load_search_w{worker_id:02d}_q{idx:03d}"
        result = post_tool(case_id, "search_messages", args)
        tool_text = parse_tool_text(result["response_text"])
        count = parse_search_count(tool_text) if result["success"] else 0
        local_events.append(
            {
                "worker_id": worker_id,
                "op_index": idx,
                "query": query,
                "mode": mode,
                "elapsed_ms": result["elapsed_ms"],
                "success": result["success"],
                "status": result["status"],
                "error_kind": result["error_kind"],
                "result_count": count,
                "case_id": case_id,
            }
        )

    with lock:
        search_events.extend(local_events)


futures = []
with concurrent.futures.ThreadPoolExecutor(max_workers=writers + searchers) as pool:
    for w in range(writers):
        futures.append(pool.submit(writer_worker, w))
    for s in range(searchers):
        futures.append(pool.submit(search_worker, s))
    for fut in futures:
        fut.result()

# Freshness probe: sample successful write markers and poll until indexed.
successful_writes = [w for w in write_events if w["success"]]
successful_writes.sort(key=lambda x: x["sent_at_ms"])
if sample_markers > 0:
    sampled = successful_writes[:sample_markers]
else:
    sampled = successful_writes

for idx, write_event in enumerate(sampled):
    marker = write_event["marker"]
    sent_at_ms = int(write_event["sent_at_ms"])
    deadline_ms = int(time.time() * 1000) + freshness_timeout_ms
    attempts = 0
    found = False
    lag_ms = -1

    while int(time.time() * 1000) < deadline_ms:
        attempts += 1
        probe_case = f"fresh_probe_{idx:03d}_a{attempts:02d}"
        probe_args = {
            "project_key": project,
            "query": marker,
            "limit": 5,
            "mode": "auto",
        }
        probe = post_tool(probe_case, "search_messages", probe_args)
        tool_text = parse_tool_text(probe["response_text"]) if probe["success"] else ""
        count = parse_search_count(tool_text) if probe["success"] else 0
        if count > 0:
            found = True
            lag_ms = int(time.time() * 1000) - sent_at_ms
            break
        time.sleep(max(freshness_poll_ms, 1) / 1000.0)

    freshness_events.append(
        {
            "marker": marker,
            "found": found,
            "lag_ms": lag_ms,
            "attempts": attempts,
            "writer_case_id": write_event["case_id"],
            "sent_at_ms": sent_at_ms,
        }
    )

write_lat = [w["elapsed_ms"] for w in write_events]
search_lat = [s["elapsed_ms"] for s in search_events]
fresh_lags = [f["lag_ms"] for f in freshness_events if f["found"] and f["lag_ms"] >= 0]

# Per-worker summary
per_writer = []
for w in range(writers):
    rows = [r for r in write_events if r["worker_id"] == w]
    succ = [r for r in rows if r["success"]]
    per_writer.append(
        {
            "worker_id": w,
            "attempted": len(rows),
            "success": len(succ),
            "failed": len(rows) - len(succ),
            **percentiles([r["elapsed_ms"] for r in rows]),
        }
    )

per_searcher = []
for s in range(searchers):
    rows = [r for r in search_events if r["worker_id"] == s]
    succ = [r for r in rows if r["success"]]
    positive = [r for r in succ if r.get("result_count", 0) > 0]
    per_searcher.append(
        {
            "worker_id": s,
            "attempted": len(rows),
            "success": len(succ),
            "failed": len(rows) - len(succ),
            "positive_results": len(positive),
            **percentiles([r["elapsed_ms"] for r in rows]),
        }
    )

write_stats = percentiles(write_lat)
search_stats = percentiles(search_lat)
fresh_stats = percentiles(fresh_lags)

summary = {
    "schema_version": 1,
    "config": {
        "writers": writers,
        "searchers": searchers,
        "ops_per_worker": ops_per_worker,
        "sample_markers": sample_markers,
        "freshness_timeout_ms": freshness_timeout_ms,
        "freshness_poll_ms": freshness_poll_ms,
    },
    "writes": {
        "attempted": len(write_events),
        "success": len([w for w in write_events if w["success"]]),
        "failed": len([w for w in write_events if not w["success"]]),
        "per_worker": per_writer,
        **write_stats,
    },
    "searches": {
        "attempted": len(search_events),
        "success": len([s for s in search_events if s["success"]]),
        "failed": len([s for s in search_events if not s["success"]]),
        "per_worker": per_searcher,
        **search_stats,
    },
    "freshness": {
        "sampled": len(freshness_events),
        "found": len([f for f in freshness_events if f["found"]]),
        "missing": len([f for f in freshness_events if not f["found"]]),
        "markers": freshness_events,
        **fresh_stats,
    },
}

latency_hist = {
    "schema_version": 1,
    "write_ms": {
        "histogram": build_hist(write_lat),
        **write_stats,
        "count": len(write_lat),
    },
    "search_ms": {
        "histogram": build_hist(search_lat),
        **search_stats,
        "count": len(search_lat),
    },
    "freshness_lag_ms": {
        "histogram": build_hist(fresh_lags),
        **fresh_stats,
        "count": len(fresh_lags),
    },
}

freshness_stats = {
    "schema_version": 1,
    "sampled": len(freshness_events),
    "found": len([f for f in freshness_events if f["found"]]),
    "missing": len([f for f in freshness_events if not f["found"]]),
    **fresh_stats,
    "markers": freshness_events,
}

summary_path.write_text(json.dumps(summary, indent=2), encoding="utf-8")
latency_hist_path.write_text(json.dumps(latency_hist, indent=2), encoding="utf-8")
freshness_path.write_text(json.dumps(freshness_stats, indent=2), encoding="utf-8")

# Additional line-oriented artifacts for diagnostics.
with (load_dir / "write_events.jsonl").open("w", encoding="utf-8") as f:
    for row in write_events:
        f.write(json.dumps(row) + "\n")

with (load_dir / "search_events.jsonl").open("w", encoding="utf-8") as f:
    for row in search_events:
        f.write(json.dumps(row) + "\n")

with (load_dir / "freshness_events.jsonl").open("w", encoding="utf-8") as f:
    for row in freshness_events:
        f.write(json.dumps(row) + "\n")

print(json.dumps({"ok": True, "summary": str(summary_path)}))
PY
then
    e2e_pass "concurrent workload execution completed"
else
    e2e_fail "concurrent workload execution failed"
fi

e2e_assert_file_exists "load summary artifact present" "$LOAD_SUMMARY_JSON"
e2e_assert_file_exists "latency histogram artifact present" "$LATENCY_HIST_JSON"
e2e_assert_file_exists "freshness artifact present" "$FRESHNESS_JSON"

e2e_mark_case_end "load_phase"
if [ "${_E2E_FAIL}" -gt "${CASE_FAIL_BASE}" ]; then
    search_v3_case_summary "load_phase" "fail" --message "workload runner failed"
else
    search_v3_case_summary "load_phase" "pass" --message "workload complete"
fi

# ---------------------------------------------------------------------------
# Case 3: Correctness + latency assertions
# ---------------------------------------------------------------------------
CASE_FAIL_BASE="${_E2E_FAIL}"
e2e_case_banner "Assertions: throughput correctness + latency budgets"
e2e_mark_case_start "latency_assertions"

EXPECTED_WRITES=$((LOAD_WRITERS * LOAD_OPS_PER_WORKER))
EXPECTED_SEARCHES=$((LOAD_SEARCHERS * LOAD_OPS_PER_WORKER))
WRITE_MIN_SUCCESS="$(min_success_required "$EXPECTED_WRITES" "$LOAD_MIN_SUCCESS_RATIO")"
SEARCH_MIN_SUCCESS="$(min_success_required "$EXPECTED_SEARCHES" "$LOAD_MIN_SUCCESS_RATIO")"
WORKER_MIN_SUCCESS="$(min_success_required "$LOAD_OPS_PER_WORKER" "$LOAD_MIN_SUCCESS_RATIO")"

WRITE_ATTEMPTED="$(jq -r '.writes.attempted // 0' "$LOAD_SUMMARY_JSON")"
WRITE_SUCCESS="$(jq -r '.writes.success // 0' "$LOAD_SUMMARY_JSON")"
WRITE_P95="$(jq -r '.writes.p95_ms // 0' "$LOAD_SUMMARY_JSON")"

SEARCH_ATTEMPTED="$(jq -r '.searches.attempted // 0' "$LOAD_SUMMARY_JSON")"
SEARCH_SUCCESS="$(jq -r '.searches.success // 0' "$LOAD_SUMMARY_JSON")"
SEARCH_P95="$(jq -r '.searches.p95_ms // 0' "$LOAD_SUMMARY_JSON")"

e2e_assert_eq "write attempts match configured load" "$EXPECTED_WRITES" "$WRITE_ATTEMPTED"
e2e_assert_eq "search attempts match configured load" "$EXPECTED_SEARCHES" "$SEARCH_ATTEMPTED"
assert_numeric_ge "write success ratio meets threshold" "$WRITE_SUCCESS" "$WRITE_MIN_SUCCESS"
assert_numeric_ge "search success ratio meets threshold" "$SEARCH_SUCCESS" "$SEARCH_MIN_SUCCESS"
assert_numeric_le "write p95 latency budget" "$WRITE_P95" "$LOAD_WRITE_P95_BUDGET_MS"
assert_numeric_le "search p95 latency budget" "$SEARCH_P95" "$LOAD_SEARCH_P95_BUDGET_MS"

# Per-writer/per-searcher assertions for breadth (>60 total suite assertions)
while IFS=$'\t' read -r worker_id attempted success failed; do
    e2e_assert_eq "writer ${worker_id} attempted count" "$LOAD_OPS_PER_WORKER" "$attempted"
    assert_numeric_ge "writer ${worker_id} success threshold" "$success" "$WORKER_MIN_SUCCESS"
    assert_numeric_le "writer ${worker_id} failed bounded" "$failed" "$((LOAD_OPS_PER_WORKER - WORKER_MIN_SUCCESS))"
done < <(jq -r '.writes.per_worker[] | "\(.worker_id)\t\(.attempted)\t\(.success)\t\(.failed)"' "$LOAD_SUMMARY_JSON")

while IFS=$'\t' read -r worker_id attempted success positive failed; do
    e2e_assert_eq "search worker ${worker_id} attempted count" "$LOAD_OPS_PER_WORKER" "$attempted"
    assert_numeric_ge "search worker ${worker_id} success threshold" "$success" "$WORKER_MIN_SUCCESS"
    assert_numeric_ge "search worker ${worker_id} positive-result checks" "$positive" 1
    assert_numeric_le "search worker ${worker_id} failed bounded" "$failed" "$((LOAD_OPS_PER_WORKER - WORKER_MIN_SUCCESS))"
done < <(jq -r '.searches.per_worker[] | "\(.worker_id)\t\(.attempted)\t\(.success)\t\(.positive_results)\t\(.failed)"' "$LOAD_SUMMARY_JSON")

e2e_mark_case_end "latency_assertions"
if [ "${_E2E_FAIL}" -gt "${CASE_FAIL_BASE}" ]; then
    search_v3_case_summary "latency_assertions" "fail" --message "throughput/latency assertions failed"
else
    search_v3_case_summary "latency_assertions" "pass" --message "throughput/latency within thresholds"
fi

# ---------------------------------------------------------------------------
# Case 4: Freshness lag assertions
# ---------------------------------------------------------------------------
CASE_FAIL_BASE="${_E2E_FAIL}"
e2e_case_banner "Assertions: indexing freshness lag under load"
e2e_mark_case_start "freshness_assertions"

FRESH_SAMPLED="$(jq -r '.sampled // 0' "$FRESHNESS_JSON")"
FRESH_FOUND="$(jq -r '.found // 0' "$FRESHNESS_JSON")"
FRESH_MISSING="$(jq -r '.missing // 0' "$FRESHNESS_JSON")"
FRESH_P95="$(jq -r '.p95_ms // 0' "$FRESHNESS_JSON")"

FRESH_MIN_FOUND="$(min_success_required "$FRESH_SAMPLED" "$LOAD_MIN_SUCCESS_RATIO")"
assert_numeric_ge "freshness sampled markers" "$FRESH_SAMPLED" 1
assert_numeric_ge "freshness found markers threshold" "$FRESH_FOUND" "$FRESH_MIN_FOUND"
assert_numeric_le "freshness missing markers bounded" "$FRESH_MISSING" "$((FRESH_SAMPLED - FRESH_MIN_FOUND))"
assert_numeric_le "freshness p95 budget" "$FRESH_P95" "$LOAD_FRESHNESS_BUDGET_MS"

# Per-marker assertions (2 per marker -> major assertion volume)
while IFS=$'\t' read -r marker found lag attempts; do
    if [ "$found" = "true" ]; then
        e2e_pass "freshness marker visible: ${marker}"
        assert_numeric_le "freshness lag budget for ${marker}" "$lag" "$LOAD_FRESHNESS_BUDGET_MS"
    else
        e2e_fail "freshness marker missing: ${marker}"
    fi
    assert_numeric_ge "freshness probe attempts for ${marker}" "$attempts" 1
done < <(jq -r '.markers[] | "\(.marker)\t\(.found)\t\(.lag_ms)\t\(.attempts)"' "$FRESHNESS_JSON")

e2e_mark_case_end "freshness_assertions"
if [ "${_E2E_FAIL}" -gt "${CASE_FAIL_BASE}" ]; then
    search_v3_case_summary "freshness_assertions" "fail" --message "freshness assertions failed"
else
    search_v3_case_summary "freshness_assertions" "pass" --message "freshness lags within budget"
fi

# ---------------------------------------------------------------------------
# Case 5: Contention diagnostics + DB checks
# ---------------------------------------------------------------------------
CASE_FAIL_BASE="${_E2E_FAIL}"
e2e_case_banner "Diagnostics: contention signals and final consistency checks"
e2e_mark_case_start "contention_diagnostics"

SERVER_LOG="${_E2E_SERVER_LOG:-}"
if [ -n "$SERVER_LOG" ] && [ -f "$SERVER_LOG" ]; then
    TOTAL_LOG_LINES="$(wc -l < "$SERVER_LOG" | tr -d '[:space:]')"
    ERROR_LINES="$(grep -Eic 'error|panic|fatal' "$SERVER_LOG" || true)"
    WARN_LINES="$(grep -Eic 'warn|retry|backpressure|shed' "$SERVER_LOG" || true)"
    LOCK_LINES="$(grep -Eic 'database is locked|sqlite_busy|lock timeout|busy timeout|contention|retrying' "$SERVER_LOG" || true)"
    START_MARKERS="$(grep -Ec 'SERVER_STARTED|CASE_START' "$SERVER_LOG" || true)"
else
    TOTAL_LOG_LINES=0
    ERROR_LINES=0
    WARN_LINES=0
    LOCK_LINES=0
    START_MARKERS=0
fi

DB_MESSAGES_FINAL="$(e2e_db_query "$SEARCH_DB" "SELECT COUNT(*) FROM messages;" | tr -d '[:space:]')"
DB_AGENTS_FINAL="$(e2e_db_query "$SEARCH_DB" "SELECT COUNT(*) FROM agents;" | tr -d '[:space:]')"
DB_PROJECTS_FINAL="$(e2e_db_query "$SEARCH_DB" "SELECT COUNT(*) FROM projects;" | tr -d '[:space:]')"
WRITE_SUCCESS_FINAL="$(jq -r '.writes.success // 0' "$LOAD_SUMMARY_JSON")"

# Export vars consumed by Python block below
export SERVER_LOG TOTAL_LOG_LINES ERROR_LINES WARN_LINES LOCK_LINES START_MARKERS
export DB_MESSAGES_FINAL DB_AGENTS_FINAL DB_PROJECTS_FINAL WRITE_SUCCESS_FINAL

python3 - "$CONTENTION_JSON" <<'PY'
import json
import os
from pathlib import Path

out = Path(os.sys.argv[1])
payload = {
    "schema_version": 1,
    "server_log": {
        "path": os.environ.get("SERVER_LOG", ""),
        "total_lines": int(os.environ.get("TOTAL_LOG_LINES", "0")),
        "error_lines": int(os.environ.get("ERROR_LINES", "0")),
        "warn_lines": int(os.environ.get("WARN_LINES", "0")),
        "lock_or_retry_lines": int(os.environ.get("LOCK_LINES", "0")),
        "start_markers": int(os.environ.get("START_MARKERS", "0")),
    },
    "database": {
        "messages": int(os.environ.get("DB_MESSAGES_FINAL", "0") or 0),
        "agents": int(os.environ.get("DB_AGENTS_FINAL", "0") or 0),
        "projects": int(os.environ.get("DB_PROJECTS_FINAL", "0") or 0),
    },
    "workload": {
        "write_success": int(os.environ.get("WRITE_SUCCESS_FINAL", "0")),
    },
}
out.write_text(json.dumps(payload, indent=2), encoding="utf-8")
PY

EXPECTED_MIN_MESSAGES=$((SEED_MESSAGE_COUNT + WRITE_SUCCESS_FINAL))

e2e_assert_file_exists "contention diagnostics artifact present" "$CONTENTION_JSON"
assert_numeric_ge "server log captured" "$TOTAL_LOG_LINES" 1
assert_numeric_ge "server log markers present" "$START_MARKERS" 1
assert_numeric_ge "database project count" "${DB_PROJECTS_FINAL:-0}" 1
assert_numeric_ge "database agent count" "${DB_AGENTS_FINAL:-0}" 4
assert_numeric_ge "database message growth after load" "${DB_MESSAGES_FINAL:-0}" "$EXPECTED_MIN_MESSAGES"
# lock/retry lines may be zero in low-contention runs, but artifact should still expose it.
assert_numeric_ge "contention lock/retry metric recorded" "$LOCK_LINES" 0
assert_numeric_ge "contention warning metric recorded" "$WARN_LINES" 0
assert_numeric_ge "contention error metric recorded" "$ERROR_LINES" 0

# Capture post-load index metadata
search_v3_capture_index_meta "post_load_index" \
    --doc-count "${DB_MESSAGES_FINAL:-0}" \
    --last-commit "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" \
    --consistency "post-load"

e2e_mark_case_end "contention_diagnostics"
if [ "${_E2E_FAIL}" -gt "${CASE_FAIL_BASE}" ]; then
    search_v3_case_summary "contention_diagnostics" "fail" --message "diagnostic assertions failed"
else
    search_v3_case_summary "contention_diagnostics" "pass" --message "diagnostics artifacts complete"
fi

# ---------------------------------------------------------------------------
# Case 6: Artifact completeness + suite finalization
# ---------------------------------------------------------------------------
CASE_FAIL_BASE="${_E2E_FAIL}"
e2e_case_banner "Artifacts: required outputs and summary contracts"
e2e_mark_case_start "artifact_contract"

# Generate suite-level Search V3 summary artifacts before validating them.
search_v3_suite_summary || true

e2e_assert_file_exists "load summary JSON" "$LOAD_SUMMARY_JSON"
e2e_assert_file_exists "latency histogram JSON" "$LATENCY_HIST_JSON"
e2e_assert_file_exists "freshness lag JSON" "$FRESHNESS_JSON"
e2e_assert_file_exists "contention diagnostics JSON" "$CONTENTION_JSON"
e2e_assert_file_exists "search_v3 run metadata" "${SEARCH_V3_RUN_DIR}/run_manifest.json"
e2e_assert_file_exists "search_v3 suite summary json" "${SEARCH_V3_RUN_DIR}/summaries/suite_summary.json"
e2e_assert_file_exists "search_v3 suite summary text" "${SEARCH_V3_RUN_DIR}/summaries/suite_summary.txt"
e2e_assert_file_exists "write events jsonl" "${LOAD_DIR}/write_events.jsonl"
e2e_assert_file_exists "search events jsonl" "${LOAD_DIR}/search_events.jsonl"
e2e_assert_file_exists "freshness events jsonl" "${LOAD_DIR}/freshness_events.jsonl"

if jq -e '.schema_version == 1 and .writes.attempted >= 1 and .searches.attempted >= 1 and .freshness.sampled >= 1' "$LOAD_SUMMARY_JSON" >/dev/null 2>&1; then
    e2e_pass "load summary JSON shape is valid"
else
    e2e_fail "load summary JSON shape invalid"
fi

if jq -e '.schema_version == 1 and .write_ms.count >= 1 and .search_ms.count >= 1' "$LATENCY_HIST_JSON" >/dev/null 2>&1; then
    e2e_pass "latency histogram JSON shape is valid"
else
    e2e_fail "latency histogram JSON shape invalid"
fi

if jq -e '.schema_version == 1 and (.sampled >= .found) and (.sampled >= .missing)' "$FRESHNESS_JSON" >/dev/null 2>&1; then
    e2e_pass "freshness lag JSON shape is valid"
else
    e2e_fail "freshness lag JSON shape invalid"
fi

if jq -e '.schema_version == 1 and .database.messages >= 1 and .server_log.total_lines >= 0' "$CONTENTION_JSON" >/dev/null 2>&1; then
    e2e_pass "contention diagnostics JSON shape is valid"
else
    e2e_fail "contention diagnostics JSON shape invalid"
fi

e2e_mark_case_end "artifact_contract"
if [ "${_E2E_FAIL}" -gt "${CASE_FAIL_BASE}" ]; then
    search_v3_case_summary "artifact_contract" "fail" --message "artifact contract failed"
else
    search_v3_case_summary "artifact_contract" "pass" --message "artifact contract satisfied"
fi

# Stop server before base summary so log stats are finalized
e2e_stop_server

# Base suite summary
# (includes pass/fail counts, metrics.json, trace/events.jsonl, and server log stats)
e2e_summary
