#!/usr/bin/env bash
# truth_probe_runner.sh
#
# G2: DB-vs-Surface Truth Probe Runner (br-2k3qx.7.2)
#
# Compares SQLite "truth" counts/memberships against surfaced robot outputs using
# the G1 canonical catalog and incident capture harness artifacts.
#
# Modes:
# - deterministic
# - high-cardinality
# - both
#
# Exit codes:
#   0  probes completed, no mismatches
#   1  fatal runner error
#   2  probes completed, mismatches detected

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
HARNESS_SCRIPT="${SCRIPT_DIR}/incident_capture_harness.sh"
DEFAULT_CATALOG="${PROJECT_ROOT}/docs/INCIDENT_BR_2K3QX_G1_SURFACE_QUERY_CATALOG.json"
source "${SCRIPT_DIR}/truth_oracle_lib.sh"
TRUTH_ORACLE_LOG_PREFIX="truth-probe"

MODE="both"
CATALOG_PATH="${DEFAULT_CATALOG}"
OUTPUT_DIR="${PROJECT_ROOT}/tests/artifacts/truth_probe/$(date -u '+%Y%m%d_%H%M%S')"
ROBOT_TIMEOUT_SECS=10
VERBOSE=0

# Seeder/harness minimum fixture bounds (must stay aligned with seed_truth_incident_fixture.sh).
MIN_PROJECTS=15
MIN_AGENTS=300
MIN_MESSAGES=3000
MIN_THREADS=300

# Deterministic mode defaults (valid minimum scale, reproducible)
DET_SEED=20260302
DET_PROJECTS=15
DET_AGENTS=300
DET_MESSAGES=3000
DET_THREADS=300

# High-cardinality mode defaults (incident-scale)
HIGH_SEED=20260303
HIGH_PROJECTS=15
HIGH_AGENTS=300
HIGH_MESSAGES=3200
HIGH_THREADS=320

usage() {
    cat <<'USAGE'
Usage: scripts/truth_probe_runner.sh [options]

Options:
  --mode <deterministic|high-cardinality|both>   Probe mode (default: both)
  --catalog <path>                                G1 catalog JSON path
  --output-dir <path>                             Output artifact directory
  --robot-timeout-secs <n>                        Timeout per robot command in harness

  --det-seed <n>                                  Deterministic mode seed
  --det-projects <n>                              Deterministic mode project count
  --det-agents <n>                                Deterministic mode agent count
  --det-messages <n>                              Deterministic mode message count
  --det-threads <n>                               Deterministic mode thread count

  --high-seed <n>                                 High-card mode seed
  --high-projects <n>                             High-card mode project count
  --high-agents <n>                               High-card mode agent count
  --high-messages <n>                             High-card mode message count
  --high-threads <n>                              High-card mode thread count

  --verbose                                       Verbose logging
  -h, --help                                      Show help

Outputs:
  <output-dir>/
    run_index.tsv                                 Mode metadata
    truth_probe_report.json                       Machine-readable mismatch report
    deterministic/ ...                            Harness artifacts (if mode includes deterministic)
    high_cardinality/ ...                         Harness artifacts (if mode includes high-cardinality)
USAGE
}

validate_fixture_bounds() {
    local mode_name="$1"
    local projects="$2"
    local agents="$3"
    local messages="$4"
    local threads="$5"

    [ "${projects}" -ge "${MIN_PROJECTS}" ] || die "${mode_name}: projects must be >= ${MIN_PROJECTS}"
    [ "${agents}" -ge "${MIN_AGENTS}" ] || die "${mode_name}: agents must be >= ${MIN_AGENTS}"
    [ "${messages}" -ge "${MIN_MESSAGES}" ] || die "${mode_name}: messages must be >= ${MIN_MESSAGES}"
    [ "${threads}" -ge "${MIN_THREADS}" ] || die "${mode_name}: threads must be >= ${MIN_THREADS}"
    [ "${messages}" -ge "${threads}" ] || die "${mode_name}: messages must be >= threads"
}

while [ $# -gt 0 ]; do
    case "$1" in
        --mode)
            MODE="${2:-}"
            shift 2
            ;;
        --catalog)
            CATALOG_PATH="${2:-}"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="${2:-}"
            shift 2
            ;;
        --robot-timeout-secs)
            ROBOT_TIMEOUT_SECS="${2:-}"
            shift 2
            ;;
        --det-seed)
            DET_SEED="${2:-}"
            shift 2
            ;;
        --det-projects)
            DET_PROJECTS="${2:-}"
            shift 2
            ;;
        --det-agents)
            DET_AGENTS="${2:-}"
            shift 2
            ;;
        --det-messages)
            DET_MESSAGES="${2:-}"
            shift 2
            ;;
        --det-threads)
            DET_THREADS="${2:-}"
            shift 2
            ;;
        --high-seed)
            HIGH_SEED="${2:-}"
            shift 2
            ;;
        --high-projects)
            HIGH_PROJECTS="${2:-}"
            shift 2
            ;;
        --high-agents)
            HIGH_AGENTS="${2:-}"
            shift 2
            ;;
        --high-messages)
            HIGH_MESSAGES="${2:-}"
            shift 2
            ;;
        --high-threads)
            HIGH_THREADS="${2:-}"
            shift 2
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

case "${MODE}" in
    deterministic|high-cardinality|both) ;;
    *)
        die "--mode must be one of: deterministic, high-cardinality, both"
        ;;
esac

require_non_empty "catalog path" "${CATALOG_PATH}"
require_non_empty "output dir" "${OUTPUT_DIR}"
require_non_empty "robot timeout" "${ROBOT_TIMEOUT_SECS}"
require_int_vars ROBOT_TIMEOUT_SECS DET_SEED DET_PROJECTS DET_AGENTS DET_MESSAGES DET_THREADS \
    HIGH_SEED HIGH_PROJECTS HIGH_AGENTS HIGH_MESSAGES HIGH_THREADS

[ -f "${CATALOG_PATH}" ] || die "catalog not found: ${CATALOG_PATH}"
[ -x "${HARNESS_SCRIPT}" ] || die "harness script not executable: ${HARNESS_SCRIPT}"
validate_fixture_bounds "deterministic mode" "${DET_PROJECTS}" "${DET_AGENTS}" "${DET_MESSAGES}" "${DET_THREADS}"
validate_fixture_bounds "high-cardinality mode" "${HIGH_PROJECTS}" "${HIGH_AGENTS}" "${HIGH_MESSAGES}" "${HIGH_THREADS}"

require_cmds python3 jq sqlite3 curl mcp-agent-mail am

mkdir -p "${OUTPUT_DIR}"
RUN_INDEX="${OUTPUT_DIR}/run_index.tsv"
printf 'mode\tharness_rc\tmode_output_dir\tseed\tprojects\tagents\tmessages\tthreads\telapsed_secs\n' > "${RUN_INDEX}"

run_mode() {
    local mode_name="$1"
    local seed="$2"
    local projects="$3"
    local agents="$4"
    local messages="$5"
    local threads="$6"

    local mode_dir="${OUTPUT_DIR}/${mode_name}"
    local stdout_path="${mode_dir}/harness.stdout.json"
    local stderr_path="${mode_dir}/harness.stderr.log"
    mkdir -p "${mode_dir}"

    log "running harness for mode=${mode_name} seed=${seed} p=${projects} a=${agents} m=${messages} t=${threads}"
    local started_at
    local finished_at
    local elapsed_secs
    started_at="$(date +%s)"
    local harness_args=(
        --output-dir "${mode_dir}"
        --seed "${seed}"
        --projects "${projects}"
        --agents "${agents}"
        --messages "${messages}"
        --threads "${threads}"
        --robot-timeout-secs "${ROBOT_TIMEOUT_SECS}"
        --skip-http-server
    )
    if [ "${VERBOSE}" -eq 1 ]; then
        harness_args+=(--verbose)
    fi
    set +e
    "${HARNESS_SCRIPT}" "${harness_args[@]}" > "${stdout_path}" 2> "${stderr_path}"
    local rc=$?
    set -e
    finished_at="$(date +%s)"
    elapsed_secs=$((finished_at - started_at))

    # Harness contract:
    # 0 = pass, 2 = verification mismatches, 1 = fatal.
    if [ "${rc}" -eq 1 ]; then
        die "harness failed for mode=${mode_name} (see ${stderr_path})"
    fi
    if [ "${rc}" -ne 0 ] && [ "${rc}" -ne 2 ]; then
        die "harness returned unexpected exit code ${rc} for mode=${mode_name} (see ${stderr_path})"
    fi

    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
        "${mode_name}" "${rc}" "${mode_dir}" "${seed}" "${projects}" "${agents}" "${messages}" "${threads}" "${elapsed_secs}" >> "${RUN_INDEX}"
}

case "${MODE}" in
    deterministic)
        run_mode "deterministic" "${DET_SEED}" "${DET_PROJECTS}" "${DET_AGENTS}" "${DET_MESSAGES}" "${DET_THREADS}"
        ;;
    high-cardinality)
        run_mode "high_cardinality" "${HIGH_SEED}" "${HIGH_PROJECTS}" "${HIGH_AGENTS}" "${HIGH_MESSAGES}" "${HIGH_THREADS}"
        ;;
    both)
        run_mode "deterministic" "${DET_SEED}" "${DET_PROJECTS}" "${DET_AGENTS}" "${DET_MESSAGES}" "${DET_THREADS}"
        run_mode "high_cardinality" "${HIGH_SEED}" "${HIGH_PROJECTS}" "${HIGH_AGENTS}" "${HIGH_MESSAGES}" "${HIGH_THREADS}"
        ;;
esac

python3 - "${CATALOG_PATH}" "${OUTPUT_DIR}" "${RUN_INDEX}" "${ROBOT_TIMEOUT_SECS}" <<'PY'
import csv
import json
import os
import re
import sys
from collections import defaultdict
from datetime import datetime, timezone
from typing import Any

catalog_path, output_dir, run_index, robot_timeout_secs_raw = sys.argv[1:5]
robot_timeout_secs = int(robot_timeout_secs_raw)

with open(catalog_path, "r", encoding="utf-8") as f:
    catalog = json.load(f)

def safe_load(path: str) -> Any:
    try:
        with open(path, "r", encoding="utf-8") as f:
            return json.load(f)
    except Exception:
        return None

def as_int(value: Any) -> int | None:
    if value is None:
        return None
    if isinstance(value, bool):
        return int(value)
    if isinstance(value, int):
        return value
    if isinstance(value, float):
        return int(value)
    if isinstance(value, str):
        s = value.strip()
        if not s:
            return None
        if re.fullmatch(r"-?\d+", s):
            return int(s)
    return None

def nested_get_int(obj: Any, keys: list[str]) -> int | None:
    # direct key match anywhere in a JSON tree
    def rec(node: Any) -> int | None:
        if isinstance(node, dict):
            for k in keys:
                if k in node:
                    v = as_int(node.get(k))
                    if v is not None:
                        return v
            for child in node.values():
                v = rec(child)
                if v is not None:
                    return v
        elif isinstance(node, list):
            for child in node:
                v = rec(child)
                if v is not None:
                    return v
        return None
    return rec(obj)

def list_count(payload: Any, primary_key: str) -> int | None:
    if payload is None:
        return None
    if isinstance(payload, list):
        return len(payload)
    if isinstance(payload, dict):
        data = payload.get("data", payload)
        if isinstance(data, list):
            return len(data)
        if isinstance(data, dict):
            arr = data.get(primary_key)
            if isinstance(arr, list):
                return len(arr)
            # fallback: if key is numeric summary
            value = as_int(data.get(primary_key))
            if value is not None:
                return value
    return None

def list_len(obj: Any) -> int | None:
    if isinstance(obj, list):
        return len(obj)
    return None

def has_capture_error(payload: Any) -> bool:
    return isinstance(payload, dict) and bool(payload.get("_capture_error"))

def inbox_count(payload: Any) -> int | None:
    if payload is None or has_capture_error(payload):
        return None
    data = payload.get("data", payload) if isinstance(payload, dict) else payload
    if isinstance(data, dict):
        value = as_int(data.get("count"))
        if value is None:
            value = as_int(data.get("total"))
        if value is not None:
            return value
        inbox = data.get("inbox")
        if isinstance(inbox, list):
            return len(inbox)
    if isinstance(data, list):
        return len(data)
    return None

def search_total(payload: Any) -> int | None:
    if payload is None or has_capture_error(payload):
        return None
    value = nested_get_int(payload, ["total_results", "count"])
    if value is not None:
        return value
    data = payload.get("data", payload) if isinstance(payload, dict) else payload
    if isinstance(data, dict):
        results = data.get("results")
        if isinstance(results, list):
            return len(results)
    return None

def compute_severity(db_truth: int, surface: int, comparator: str) -> str:
    if comparator == "lte":
        # only mismatch is surface > db_truth
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
        # only mismatch is surface < db_truth
        deficit = max(0, db_truth - surface)
        if db_truth > 0 and surface == 0:
            return "critical"
        ratio = deficit / max(db_truth, 1)
        if ratio >= 0.50:
            return "high"
        if ratio >= 0.10:
            return "medium"
        return "low"

    # eq comparator
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
    mode_meta: dict[str, Any],
    surface_id: str,
    metric: str,
    db_truth: int | None,
    surface_value: int | None,
    comparator: str,
    snapshot: str,
    note: str | None = None,
    regression_class: str | None = None,
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
        else:
            matched = None
            status = "indeterminate"
            severity = "none"
        if matched is True:
            status = "match"
            severity = "none"
        elif matched is False:
            status = "mismatch"
            severity = compute_severity(db_truth, surface_value, comparator)

    repro = {
        "mode": mode_meta["mode"],
        "seed": mode_meta["seed"],
        "fixture_params": {
            "projects": mode_meta["projects"],
            "agents": mode_meta["agents"],
            "messages": mode_meta["messages"],
            "threads": mode_meta["threads"],
        },
        "mode_output_dir": mode_meta["mode_output_dir"],
        "repro_script": os.path.join(mode_meta["mode_output_dir"], "repro.sh"),
        "snapshot": snapshot,
        "surface_id": surface_id,
    }

    result = {
        "check_id": f"{surface_id}:{metric}",
        "mode": mode_meta["mode"],
        "surface_id": surface_id,
        "metric": metric,
        "comparator": comparator,
        "db_truth": db_truth,
        "surface_value": surface_value,
        "matched": matched,
        "status": status,
        "severity": severity,
        "snapshot": snapshot,
        "repro": repro,
        "note": note,
    }
    if regression_class:
        result["regression_class"] = regression_class
    return result

# ── E5 (br-2k3qx.5.5): Incident regression class helpers ──────────────────
BODY_PLACEHOLDER_PATTERNS = [
    "(empty body)", "(loading...)", "(no body)", "(body unavailable)",
    "...", "(placeholder)", "(pending)",
]

def has_body_content(obj: Any, body_keys: list[str] | None = None) -> bool:
    """Check if an object contains non-empty, non-placeholder body text."""
    if body_keys is None:
        body_keys = ["body_md", "body", "body_excerpt", "body_text"]
    if not isinstance(obj, dict):
        return False
    for key in body_keys:
        val = obj.get(key)
        if isinstance(val, str) and len(val.strip()) > 0:
            stripped = val.strip().lower()
            if stripped not in [p.lower() for p in BODY_PLACEHOLDER_PATTERNS]:
                return True
    return False

def count_messages_with_body(payload: Any) -> tuple[int, int]:
    """Returns (total_messages, messages_with_body) from inbox/list payloads."""
    messages = []
    if isinstance(payload, list):
        messages = payload
    elif isinstance(payload, dict):
        data = payload.get("data", payload)
        if isinstance(data, list):
            messages = data
        elif isinstance(data, dict):
            for k in ("inbox", "messages", "items", "results"):
                arr = data.get(k)
                if isinstance(arr, list):
                    messages = arr
                    break
    total = len(messages)
    with_body = sum(1 for m in messages if has_body_content(m))
    return (total, with_body)

def has_markdown_markers(text: str) -> bool:
    """Check if text contains at least one markdown structural element."""
    markers = ["# ", "## ", "### ", "**", "*", "```", "- ", "1. ", "| ", "> "]
    return any(marker in text for marker in markers)

def extract_auth_status(payload: Any) -> dict[str, Any]:
    """Extract auth-related fields from system health snapshot."""
    result: dict[str, Any] = {"auth_enabled": None, "health_url": None, "remediation": None}
    if not isinstance(payload, dict):
        return result
    data = payload.get("data", payload) if isinstance(payload, dict) else payload
    if isinstance(data, dict):
        result["auth_enabled"] = data.get("auth_enabled", data.get("bearer_auth_enabled"))
        result["health_url"] = data.get("web_ui_url", data.get("health_url"))
        result["remediation"] = data.get("remediation", data.get("auth_remediation"))
    return result

mode_runs = []
with open(run_index, "r", encoding="utf-8") as f:
    reader = csv.DictReader(f, delimiter="\t")
    for row in reader:
        mode_runs.append(
            {
                "mode": row["mode"],
                "harness_rc": int(row["harness_rc"]),
                "mode_output_dir": row["mode_output_dir"],
                "seed": int(row["seed"]),
                "projects": int(row["projects"]),
                "agents": int(row["agents"]),
                "messages": int(row["messages"]),
                "threads": int(row["threads"]),
                "elapsed_secs": int(row.get("elapsed_secs", 0)),
            }
        )

surface_results: list[dict[str, Any]] = []
robot_family_results: list[dict[str, Any]] = []
stress_mode_results: list[dict[str, Any]] = []
checks_all: list[dict[str, Any]] = []
mismatches: list[dict[str, Any]] = []
indeterminate_checks: list[dict[str, Any]] = []

for mode_meta in mode_runs:
    mode_dir = mode_meta["mode_output_dir"]
    snap_dir = os.path.join(mode_dir, "snapshots")
    diag_dir = os.path.join(mode_dir, "diagnostics")
    db_truth = safe_load(os.path.join(diag_dir, "db_truth.json")) or {}
    gc = db_truth.get("global_counts", {})

    projects_view = safe_load(os.path.join(snap_dir, "projects_view.json"))
    agents_view = safe_load(os.path.join(snap_dir, "agents_view.json"))
    dashboard_status = safe_load(os.path.join(snap_dir, "dashboard_status.json"))
    messages_inbox = safe_load(os.path.join(snap_dir, "messages_inbox.json"))
    messages_inbox_limit5 = safe_load(os.path.join(snap_dir, "messages_inbox_limit5.json"))
    messages_inbox_unread = safe_load(os.path.join(snap_dir, "messages_inbox_unread.json"))
    threads_view = safe_load(os.path.join(snap_dir, "threads_view.json"))
    threads_view_limit3 = safe_load(os.path.join(snap_dir, "threads_view_limit3.json"))
    search_results = safe_load(os.path.join(snap_dir, "search_results.json"))
    search_results_since = safe_load(os.path.join(snap_dir, "search_results_since.json"))
    search_results_kind_message = safe_load(os.path.join(snap_dir, "search_results_kind_message.json"))
    reservations_view = safe_load(os.path.join(snap_dir, "reservations.json"))
    metrics_view = safe_load(os.path.join(snap_dir, "metrics.json"))
    system_health = safe_load(os.path.join(snap_dir, "system_health.json"))
    timeline_view = safe_load(os.path.join(snap_dir, "timeline.json"))
    contacts_view = safe_load(os.path.join(snap_dir, "contacts.json"))
    analytics_view = safe_load(os.path.join(snap_dir, "analytics.json"))
    attachments_view = safe_load(os.path.join(snap_dir, "attachments.json"))
    navigate_view = safe_load(os.path.join(snap_dir, "navigate.json"))
    overview_view = safe_load(os.path.join(snap_dir, "overview.json"))
    message_detail = safe_load(os.path.join(snap_dir, "message_detail.json"))
    context_view = safe_load(os.path.join(mode_dir, "context.json"))
    truth_comparison = safe_load(os.path.join(diag_dir, "truth_comparison.json"))

    db_projects = as_int(gc.get("projects"))
    db_agents = as_int(gc.get("agents"))
    db_messages = as_int(gc.get("messages"))
    db_threads = as_int(gc.get("threads"))
    db_file_reservations = as_int(gc.get("file_reservations"))

    required_snapshots: dict[str, Any] = {
        "dashboard_status.json": dashboard_status,
        "messages_inbox.json": messages_inbox,
        "messages_inbox_limit5.json": messages_inbox_limit5,
        "messages_inbox_unread.json": messages_inbox_unread,
        "timeline.json": timeline_view,
        "overview.json": overview_view,
        "threads_view.json": threads_view,
        "threads_view_limit3.json": threads_view_limit3,
        "search_results.json": search_results,
        "search_results_since.json": search_results_since,
        "search_results_kind_message.json": search_results_kind_message,
        "message_detail.json": message_detail,
        "navigate.json": navigate_view,
        "reservations.json": reservations_view,
        "metrics.json": metrics_view,
        "system_health.json": system_health,
        "analytics.json": analytics_view,
        "agents_view.json": agents_view,
        "contacts.json": contacts_view,
        "projects_view.json": projects_view,
        "attachments.json": attachments_view,
    }
    for snapshot_name, payload in required_snapshots.items():
        if payload is None:
            present = 0
            note = "snapshot missing or invalid JSON"
        elif has_capture_error(payload):
            present = 0
            note = "snapshot capture_error"
        else:
            present = 1
            note = None
        check = build_check(
            mode_meta=mode_meta,
            surface_id=f"artifact.{mode_meta['mode']}",
            metric=f"{snapshot_name}.present",
            db_truth=1,
            surface_value=present,
            comparator="eq",
            snapshot=snapshot_name,
            note=note,
        )
        checks_all.append(check)
        if check["status"] == "mismatch":
            mismatches.append(check)
        elif check["status"] == "indeterminate":
            indeterminate_checks.append(check)

    def min_required(total: int | None) -> int:
        return 1 if (total is not None and total > 0) else 0

    for surface in catalog.get("tui_surfaces", []):
        surface_id = surface.get("surface_id", "unknown")
        checks: list[dict[str, Any]] = []
        skipped_reason = None

        if surface_id == "tui.projects":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="projects.count",
                    db_truth=db_projects,
                    surface_value=list_count(projects_view, "projects"),
                    comparator="eq",
                    snapshot="projects_view.json",
                )
            )
        elif surface_id == "tui.agents":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="agents.count_lte_global",
                    db_truth=db_agents,
                    surface_value=list_count(agents_view, "agents"),
                    comparator="lte",
                    snapshot="agents_view.json",
                )
            )
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="agents.count_gte_one_if_any_agents",
                    db_truth=min_required(db_agents),
                    surface_value=list_count(agents_view, "agents"),
                    comparator="gte",
                    snapshot="agents_view.json",
                )
            )
        elif surface_id == "tui.dashboard":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="dashboard.top_threads_gte_one_if_any_threads",
                    db_truth=min_required(db_threads),
                    surface_value=list_len((dashboard_status or {}).get("top_threads")),
                    comparator="gte",
                    snapshot="dashboard_status.json",
                )
            )
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="dashboard.recent_messages_nonnegative",
                    db_truth=0,
                    surface_value=nested_get_int(dashboard_status, ["recent_messages"]),
                    comparator="gte",
                    snapshot="dashboard_status.json",
                )
            )
        elif surface_id == "tui.messages":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="messages.inbox_total_lte_global",
                    db_truth=db_messages,
                    surface_value=nested_get_int(messages_inbox, ["count", "total", "messages_total"]),
                    comparator="lte",
                    snapshot="messages_inbox.json",
                )
            )
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="messages.inbox_total_gte_one_if_any_messages",
                    db_truth=min_required(db_messages),
                    surface_value=nested_get_int(messages_inbox, ["count", "total", "messages_total"]),
                    comparator="gte",
                    snapshot="messages_inbox.json",
                )
            )
        elif surface_id == "tui.threads":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="threads.message_count_gte_one_if_any_threads",
                    db_truth=min_required(db_threads),
                    surface_value=nested_get_int(threads_view, ["message_count", "count"]),
                    comparator="gte",
                    snapshot="threads_view.json",
                )
            )
        elif surface_id == "tui.search":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="search.total_results_gte_one_if_any_messages",
                    db_truth=min_required(db_messages),
                    surface_value=nested_get_int(search_results, ["total_results", "count"]),
                    comparator="gte",
                    snapshot="search_results.json",
                )
            )
        elif surface_id == "tui.reservations":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="reservations.active_lte_db",
                    db_truth=db_file_reservations,
                    surface_value=nested_get_int(reservations_view, ["all_active", "count"]),
                    comparator="lte",
                    snapshot="reservations.json",
                )
            )
        elif surface_id == "tui.tool_metrics":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="tool_metrics.total_calls_nonnegative",
                    db_truth=0,
                    surface_value=nested_get_int(metrics_view, ["total_calls", "count"]),
                    comparator="gte",
                    snapshot="metrics.json",
                )
            )
        elif surface_id == "tui.system_health":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="system_health.probes_count_gte_one",
                    db_truth=1,
                    surface_value=list_len((system_health or {}).get("probes")),
                    comparator="gte",
                    snapshot="system_health.json",
                )
            )
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="system_health.circuits_count_nonnegative",
                    db_truth=0,
                    surface_value=list_len((system_health or {}).get("circuits")),
                    comparator="gte",
                    snapshot="system_health.json",
                )
            )
        elif surface_id == "tui.timeline":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="timeline.event_count_nonnegative",
                    db_truth=0,
                    surface_value=nested_get_int(timeline_view, ["count"]),
                    comparator="gte",
                    snapshot="timeline.json",
                )
            )
        elif surface_id == "tui.contacts":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="contacts.count_lte_global_agents",
                    db_truth=db_agents,
                    surface_value=nested_get_int(contacts_view, ["count"]),
                    comparator="lte",
                    snapshot="contacts.json",
                )
            )
        elif surface_id == "tui.explorer":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="explorer.proxy_inbox_count_lte_global_messages",
                    db_truth=db_messages,
                    surface_value=nested_get_int(messages_inbox, ["count", "total", "messages_total"]),
                    comparator="lte",
                    snapshot="messages_inbox.json",
                    note="explorer proxy check via inbox snapshot",
                )
            )
        elif surface_id == "tui.analytics":
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="analytics.anomaly_count_nonnegative",
                    db_truth=0,
                    surface_value=nested_get_int(analytics_view, ["anomaly_count", "count"]),
                    comparator="gte",
                    snapshot="analytics.json",
                )
            )
        elif surface_id == "tui.attachments":
            attachments_value = None
            attachments_note = None
            if attachments_view is None:
                attachments_note = "attachments snapshot missing"
            elif has_capture_error(attachments_view):
                attachments_note = "attachments snapshot capture_error"
            else:
                attachments_value = nested_get_int(attachments_view, ["count", "total"])
                if attachments_value is None:
                    attachments_value = list_count(attachments_view, "attachments")
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="attachments.inventory_count_nonnegative",
                    db_truth=0,
                    surface_value=attachments_value,
                    comparator="gte",
                    snapshot="attachments.json",
                    note=attachments_note,
                )
            )
        elif surface_id == "tui.archive_browser":
            navigate_value = None
            navigate_note = None
            if navigate_view is None:
                navigate_note = "navigate snapshot missing"
            elif has_capture_error(navigate_view):
                navigate_note = "navigate snapshot capture_error"
            else:
                navigate_value = 1
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=surface_id,
                    metric="archive_browser.navigate_response_present",
                    db_truth=1,
                    surface_value=navigate_value,
                    comparator="eq",
                    snapshot="navigate.json",
                    note=navigate_note,
                )
            )
        else:
            skipped_reason = "no probe adapter implemented for this surface yet"

        if checks:
            for check in checks:
                checks_all.append(check)
                if check["status"] == "mismatch":
                    mismatches.append(check)
                elif check["status"] == "indeterminate":
                    indeterminate_checks.append(check)
            surface_results.append(
                {
                    "mode": mode_meta["mode"],
                    "surface_id": surface_id,
                    "status": "probed",
                    "checks": checks,
                }
            )
        else:
            surface_results.append(
                {
                    "mode": mode_meta["mode"],
                    "surface_id": surface_id,
                    "status": "skipped",
                    "checks": [],
                    "skipped_reason": skipped_reason,
                }
            )

    # ── E5 (br-2k3qx.5.5): Incident-specific regression class checks ────
    # These checks are tagged with regression_class for CI gate classification.

    # --- Regression Class: false_empty ---
    # Detect surfaces that show zero items when DB has data.
    false_empty_surfaces = [
        ("incident.dashboard", "false_empty.recent_messages",
         min_required(db_messages),
         nested_get_int(dashboard_status, ["recent_messages"]),
         "gte", "dashboard_status.json"),
        ("incident.messages", "false_empty.inbox_count",
         min_required(db_messages),
         nested_get_int(messages_inbox, ["count", "total", "messages_total"]),
         "gte", "messages_inbox.json"),
        ("incident.threads", "false_empty.thread_count",
         min_required(db_threads),
         nested_get_int(threads_view, ["message_count", "count"]),
         "gte", "threads_view.json"),
        ("incident.agents", "false_empty.agent_count",
         min_required(db_agents),
         list_count(agents_view, "agents"),
         "gte", "agents_view.json"),
        ("incident.projects", "false_empty.project_count",
         min_required(db_projects),
         list_count(projects_view, "projects"),
         "gte", "projects_view.json"),
    ]
    for sid, metric, db_val, surface_val, cmp, snap in false_empty_surfaces:
        check = build_check(
            mode_meta=mode_meta,
            surface_id=sid,
            metric=metric,
            db_truth=db_val,
            surface_value=surface_val,
            comparator=cmp,
            snapshot=snap,
            regression_class="false_empty",
        )
        checks_all.append(check)
        if check["status"] == "mismatch":
            mismatches.append(check)
        elif check["status"] == "indeterminate":
            indeterminate_checks.append(check)

    # --- Regression Class: body_placeholder ---
    # Detect surfaces where message bodies are empty or contain placeholder text.
    inbox_total, inbox_with_body = count_messages_with_body(messages_inbox)
    body_placeholder_present = 1 if (inbox_total > 0 and inbox_with_body > 0) else 0
    body_placeholder_expected = 1 if inbox_total > 0 else 0
    check = build_check(
        mode_meta=mode_meta,
        surface_id="incident.messages",
        metric="body_placeholder.inbox_has_bodies",
        db_truth=body_placeholder_expected,
        surface_value=body_placeholder_present,
        comparator="eq",
        snapshot="messages_inbox.json",
        regression_class="body_placeholder",
        note=f"inbox_total={inbox_total}, with_body={inbox_with_body}",
    )
    checks_all.append(check)
    if check["status"] == "mismatch":
        mismatches.append(check)
    elif check["status"] == "indeterminate":
        indeterminate_checks.append(check)

    # Check message detail body is non-empty
    detail_has_body = 0
    detail_expected = 0
    if message_detail is not None and not has_capture_error(message_detail):
        detail_expected = 1
        detail_has_body = 1 if has_body_content(
            message_detail.get("data", message_detail) if isinstance(message_detail, dict) else message_detail
        ) else 0
    check = build_check(
        mode_meta=mode_meta,
        surface_id="incident.message_detail",
        metric="body_placeholder.detail_has_body",
        db_truth=detail_expected,
        surface_value=detail_has_body,
        comparator="eq",
        snapshot="message_detail.json",
        regression_class="body_placeholder",
    )
    checks_all.append(check)
    if check["status"] == "mismatch":
        mismatches.append(check)
    elif check["status"] == "indeterminate":
        indeterminate_checks.append(check)

    # Check that at least some inbox messages contain markdown structural markers
    inbox_markdown_count = 0
    if messages_inbox is not None and not has_capture_error(messages_inbox):
        msgs_list = []
        if isinstance(messages_inbox, list):
            msgs_list = messages_inbox
        elif isinstance(messages_inbox, dict):
            d = messages_inbox.get("data", messages_inbox)
            if isinstance(d, list):
                msgs_list = d
            elif isinstance(d, dict):
                for k in ("inbox", "messages", "items"):
                    arr = d.get(k)
                    if isinstance(arr, list):
                        msgs_list = arr
                        break
        for msg in msgs_list:
            if isinstance(msg, dict):
                for bk in ("body_md", "body", "body_excerpt"):
                    bv = msg.get(bk)
                    if isinstance(bv, str) and has_markdown_markers(bv):
                        inbox_markdown_count += 1
                        break
    md_expected = 1 if inbox_total > 0 else 0
    md_present = 1 if inbox_markdown_count > 0 else 0
    check = build_check(
        mode_meta=mode_meta,
        surface_id="incident.messages",
        metric="body_placeholder.markdown_fidelity",
        db_truth=md_expected,
        surface_value=md_present,
        comparator="eq",
        snapshot="messages_inbox.json",
        regression_class="body_placeholder",
        note=f"inbox_total={inbox_total}, with_markdown={inbox_markdown_count}",
    )
    checks_all.append(check)
    if check["status"] == "mismatch":
        mismatches.append(check)
    elif check["status"] == "indeterminate":
        indeterminate_checks.append(check)

    # --- Regression Class: auth_workflow ---
    # Detect system health auth workflow regressions.
    auth_info = extract_auth_status(system_health)
    # Health probes should be present regardless of auth
    health_probes_present = 0
    if system_health is not None and not has_capture_error(system_health):
        health_probes_present = 1
    check = build_check(
        mode_meta=mode_meta,
        surface_id="incident.system_health",
        metric="auth_workflow.health_accessible",
        db_truth=1,
        surface_value=health_probes_present,
        comparator="eq",
        snapshot="system_health.json",
        regression_class="auth_workflow",
        note="system health must be accessible even with auth enabled",
    )
    checks_all.append(check)
    if check["status"] == "mismatch":
        mismatches.append(check)
    elif check["status"] == "indeterminate":
        indeterminate_checks.append(check)

    # If auth is enabled, health URL should include token context or remediation
    if auth_info["auth_enabled"]:
        has_url_or_remediation = 0
        if auth_info["health_url"] or auth_info["remediation"]:
            has_url_or_remediation = 1
        check = build_check(
            mode_meta=mode_meta,
            surface_id="incident.system_health",
            metric="auth_workflow.url_or_remediation_present",
            db_truth=1,
            surface_value=has_url_or_remediation,
            comparator="eq",
            snapshot="system_health.json",
            regression_class="auth_workflow",
            note="when auth is enabled, health URL or remediation guidance must be present",
        )
        checks_all.append(check)
        if check["status"] == "mismatch":
            mismatches.append(check)
        elif check["status"] == "indeterminate":
            indeterminate_checks.append(check)
    # ── End E5 incident regression checks ──────────────────────────────────

    command_snapshot_map: dict[str, tuple[str, Any]] = {
        "status": ("dashboard_status.json", dashboard_status),
        "inbox": ("messages_inbox.json", messages_inbox),
        "timeline": ("timeline.json", timeline_view),
        "overview": ("overview.json", overview_view),
        "thread": ("threads_view.json", threads_view),
        "search": ("search_results.json", search_results),
        "message": ("message_detail.json", message_detail),
        "navigate": ("navigate.json", navigate_view),
        "reservations": ("reservations.json", reservations_view),
        "metrics": ("metrics.json", metrics_view),
        "health": ("system_health.json", system_health),
        "analytics": ("analytics.json", analytics_view),
        "agents": ("agents_view.json", agents_view),
        "contacts": ("contacts.json", contacts_view),
        "projects": ("projects_view.json", projects_view),
        "attachments": ("attachments.json", attachments_view),
    }

    for family in catalog.get("robot_command_families", []):
        family_name = family.get("family", "unknown")
        command_rows: list[dict[str, Any]] = []
        for command in family.get("commands", []):
            command_name = command.get("name", "unknown")
            mapping = command_snapshot_map.get(command_name)
            if mapping is None:
                command_rows.append(
                    {
                        "command": command_name,
                        "status": "skipped",
                        "checks": [],
                        "skipped_reason": "no snapshot mapping for command",
                    }
                )
                continue
            snapshot_name, payload = mapping
            checks: list[dict[str, Any]] = []
            response_value = 0
            response_note = None
            if payload is None:
                response_note = "snapshot missing"
            elif has_capture_error(payload):
                response_note = "snapshot capture_error"
            else:
                response_value = 1
            checks.append(
                build_check(
                    mode_meta=mode_meta,
                    surface_id=f"robot.{command_name}",
                    metric="response.present",
                    db_truth=1,
                    surface_value=response_value,
                    comparator="eq",
                    snapshot=snapshot_name,
                    note=response_note,
                )
            )

            if command_name == "status":
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id=f"robot.{command_name}",
                        metric="top_threads.gte_one_if_any_threads",
                        db_truth=min_required(db_threads),
                        surface_value=list_len((payload or {}).get("top_threads")) if response_value else None,
                        comparator="gte",
                        snapshot=snapshot_name,
                        note=response_note,
                    )
                )
            elif command_name == "inbox":
                baseline_inbox_count = inbox_count(payload) if response_value else None
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id=f"robot.{command_name}",
                        metric="count.lte_global_messages",
                        db_truth=db_messages,
                        surface_value=nested_get_int(payload, ["count", "total"]) if response_value else None,
                        comparator="lte",
                        snapshot=snapshot_name,
                        note=response_note,
                    )
                )
                limit5_value = None
                limit5_note = None
                if messages_inbox_limit5 is None:
                    limit5_note = "messages_inbox_limit5 snapshot missing"
                elif has_capture_error(messages_inbox_limit5):
                    limit5_note = "messages_inbox_limit5 snapshot capture_error"
                else:
                    limit5_value = inbox_count(messages_inbox_limit5)
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id="robot.inbox_params",
                        metric="limit5.lte_baseline",
                        db_truth=baseline_inbox_count,
                        surface_value=limit5_value,
                        comparator="lte",
                        snapshot="messages_inbox_limit5.json",
                        note=limit5_note,
                    )
                )
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id="robot.inbox_params",
                        metric="limit5.lte_5",
                        db_truth=5,
                        surface_value=limit5_value,
                        comparator="lte",
                        snapshot="messages_inbox_limit5.json",
                        note=limit5_note,
                    )
                )

                unread_value = None
                unread_note = None
                if messages_inbox_unread is None:
                    unread_note = "messages_inbox_unread snapshot missing"
                elif has_capture_error(messages_inbox_unread):
                    unread_note = "messages_inbox_unread snapshot capture_error"
                else:
                    unread_value = inbox_count(messages_inbox_unread)
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id="robot.inbox_params",
                        metric="unread.lte_baseline",
                        db_truth=baseline_inbox_count,
                        surface_value=unread_value,
                        comparator="lte",
                        snapshot="messages_inbox_unread.json",
                        note=unread_note,
                    )
                )
            elif command_name == "overview":
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id=f"robot.{command_name}",
                        metric="project_count.gte_one_if_any_projects",
                        db_truth=min_required(db_projects),
                        surface_value=nested_get_int(payload, ["project_count", "projects_total", "count"]) if response_value else None,
                        comparator="gte",
                        snapshot=snapshot_name,
                        note=response_note,
                    )
                )
            elif command_name == "thread":
                baseline_thread_count = nested_get_int(payload, ["message_count", "count"]) if response_value else None
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id=f"robot.{command_name}",
                        metric="message_count.gte_one_if_any_threads",
                        db_truth=min_required(db_threads),
                        surface_value=nested_get_int(payload, ["message_count", "count"]) if response_value else None,
                        comparator="gte",
                        snapshot=snapshot_name,
                        note=response_note,
                    )
                )
                thread_limit3_value = None
                thread_limit3_note = None
                if threads_view_limit3 is None:
                    thread_limit3_note = "threads_view_limit3 snapshot missing"
                elif has_capture_error(threads_view_limit3):
                    thread_limit3_note = "threads_view_limit3 snapshot capture_error"
                else:
                    thread_limit3_value = nested_get_int(threads_view_limit3, ["message_count", "count"])
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id="robot.thread_params",
                        metric="limit3.lte_baseline",
                        db_truth=baseline_thread_count,
                        surface_value=thread_limit3_value,
                        comparator="lte",
                        snapshot="threads_view_limit3.json",
                        note=thread_limit3_note,
                    )
                )
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id="robot.thread_params",
                        metric="limit3.lte_3",
                        db_truth=3,
                        surface_value=thread_limit3_value,
                        comparator="lte",
                        snapshot="threads_view_limit3.json",
                        note=thread_limit3_note,
                    )
                )
            elif command_name == "search":
                baseline_search_total = search_total(payload) if response_value else None
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id=f"robot.{command_name}",
                        metric="total_results.gte_one_if_any_messages",
                        db_truth=min_required(db_messages),
                        surface_value=nested_get_int(payload, ["total_results", "count"]) if response_value else None,
                        comparator="gte",
                        snapshot=snapshot_name,
                        note=response_note,
                    )
                )
                search_since_value = None
                search_since_note = None
                if search_results_since is None:
                    search_since_note = "search_results_since snapshot missing"
                elif has_capture_error(search_results_since):
                    search_since_note = "search_results_since snapshot capture_error"
                else:
                    search_since_value = search_total(search_results_since)
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id="robot.search_params",
                        metric="since.lte_baseline",
                        db_truth=baseline_search_total,
                        surface_value=search_since_value,
                        comparator="lte",
                        snapshot="search_results_since.json",
                        note=search_since_note,
                    )
                )

                search_kind_value = None
                search_kind_note = None
                if search_results_kind_message is None:
                    search_kind_note = "search_results_kind_message snapshot missing"
                elif has_capture_error(search_results_kind_message):
                    search_kind_note = "search_results_kind_message snapshot capture_error"
                else:
                    search_kind_value = search_total(search_results_kind_message)
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id="robot.search_params",
                        metric="kind_message.lte_baseline",
                        db_truth=baseline_search_total,
                        surface_value=search_kind_value,
                        comparator="lte",
                        snapshot="search_results_kind_message.json",
                        note=search_kind_note,
                    )
                )
            elif command_name == "health":
                checks.append(
                    build_check(
                        mode_meta=mode_meta,
                        surface_id=f"robot.{command_name}",
                        metric="probes_count.gte_one",
                        db_truth=1,
                        surface_value=list_len((payload or {}).get("probes")) if response_value else None,
                        comparator="gte",
                        snapshot=snapshot_name,
                        note=response_note,
                    )
                )

            for check in checks:
                checks_all.append(check)
                if check["status"] == "mismatch":
                    mismatches.append(check)
                elif check["status"] == "indeterminate":
                    indeterminate_checks.append(check)
            command_rows.append(
                {
                    "command": command_name,
                    "status": "probed",
                    "checks": checks,
                }
            )
        robot_family_results.append(
            {
                "mode": mode_meta["mode"],
                "family": family_name,
                "commands": command_rows,
            }
        )

    per_project = db_truth.get("per_project")
    per_project_count = None
    if isinstance(per_project, dict):
        per_project_count = len([k for k in per_project.keys() if k != "_error"])
    capture_failures = nested_get_int(context_view, ["failed"])
    if capture_failures is None:
        capture_failures = -1
    capture_successes = nested_get_int(context_view, ["succeeded"])
    if capture_successes is None:
        capture_successes = -1
    truth_mismatch_count = nested_get_int(truth_comparison, ["mismatch_count"])
    runtime_budget_secs = max(180, robot_timeout_secs * 30)

    stress_checks: list[dict[str, Any]] = []
    stress_checks.append(
        build_check(
            mode_meta=mode_meta,
            surface_id=f"stress.{mode_meta['mode']}",
            metric="harness.elapsed_secs.lte_budget",
            db_truth=runtime_budget_secs,
            surface_value=mode_meta["elapsed_secs"],
            comparator="lte",
            snapshot="run_index.tsv",
            note=f"robot_timeout_secs={robot_timeout_secs}, derived_budget_secs={runtime_budget_secs}",
        )
    )
    stress_checks.append(
        build_check(
            mode_meta=mode_meta,
            surface_id=f"stress.{mode_meta['mode']}",
            metric="capture.failures.eq_zero",
            db_truth=0,
            surface_value=capture_failures,
            comparator="eq",
            snapshot="context.json",
        )
    )
    stress_checks.append(
        build_check(
            mode_meta=mode_meta,
            surface_id=f"stress.{mode_meta['mode']}",
            metric="truth_comparison.present",
            db_truth=1,
            surface_value=1 if truth_comparison is not None else 0,
            comparator="eq",
            snapshot="diagnostics/truth_comparison.json",
            note="missing truth comparison indicates stress run artifact failure" if truth_comparison is None else None,
        )
    )

    if mode_meta["mode"] == "high_cardinality":
        stress_checks.extend(
            [
                build_check(
                    mode_meta=mode_meta,
                    surface_id="stress.high_cardinality",
                    metric="db.projects.eq_fixture_param",
                    db_truth=mode_meta["projects"],
                    surface_value=db_projects,
                    comparator="eq",
                    snapshot="diagnostics/db_truth.json",
                ),
                build_check(
                    mode_meta=mode_meta,
                    surface_id="stress.high_cardinality",
                    metric="db.agents.eq_fixture_param",
                    db_truth=mode_meta["agents"],
                    surface_value=db_agents,
                    comparator="eq",
                    snapshot="diagnostics/db_truth.json",
                ),
                build_check(
                    mode_meta=mode_meta,
                    surface_id="stress.high_cardinality",
                    metric="db.messages.eq_fixture_param",
                    db_truth=mode_meta["messages"],
                    surface_value=db_messages,
                    comparator="eq",
                    snapshot="diagnostics/db_truth.json",
                ),
                build_check(
                    mode_meta=mode_meta,
                    surface_id="stress.high_cardinality",
                    metric="db.threads.eq_fixture_param",
                    db_truth=mode_meta["threads"],
                    surface_value=db_threads,
                    comparator="eq",
                    snapshot="diagnostics/db_truth.json",
                ),
                build_check(
                    mode_meta=mode_meta,
                    surface_id="stress.high_cardinality",
                    metric="db.per_project_count.gte_fixture_projects",
                    db_truth=mode_meta["projects"],
                    surface_value=per_project_count,
                    comparator="gte",
                    snapshot="diagnostics/db_truth.json",
                ),
                build_check(
                    mode_meta=mode_meta,
                    surface_id="stress.high_cardinality",
                    metric="capture.successes.gte_20",
                    db_truth=20,
                    surface_value=capture_successes,
                    comparator="gte",
                    snapshot="context.json",
                    note="expects full robot sweep plus http captures",
                ),
            ]
        )

    for check in stress_checks:
        checks_all.append(check)
        if check["status"] == "mismatch":
            mismatches.append(check)
        elif check["status"] == "indeterminate":
            indeterminate_checks.append(check)

    stress_mode_results.append(
        {
            "mode": mode_meta["mode"],
            "elapsed_secs": mode_meta["elapsed_secs"],
            "runtime_budget_secs": runtime_budget_secs,
            "within_budget": mode_meta["elapsed_secs"] <= runtime_budget_secs,
            "capture_failures": capture_failures,
            "capture_successes": capture_successes,
            "truth_mismatch_count": truth_mismatch_count,
            "checks": stress_checks,
        }
    )

mismatch_count = len(mismatches)
indeterminate_count = len(indeterminate_checks)
verdict = "PASS" if mismatch_count == 0 and indeterminate_count == 0 else "FAIL"
severity_counts = {"critical": 0, "high": 0, "medium": 0, "low": 0}
for mismatch in mismatches:
    severity = mismatch.get("severity")
    if severity in severity_counts:
        severity_counts[severity] += 1

culprit_surface_map: dict[str, dict[str, int]] = defaultdict(
    lambda: {"mismatch_count": 0, "critical": 0, "high": 0, "medium": 0, "low": 0}
)
for mismatch in mismatches:
    surface_id = mismatch.get("surface_id", "unknown")
    severity = mismatch.get("severity")
    culprit_surface_map[surface_id]["mismatch_count"] += 1
    if severity in ("critical", "high", "medium", "low"):
        culprit_surface_map[surface_id][severity] += 1

report = {
    "bead_id": "br-2k3qx.7.2",
    "generated_at": datetime.now(timezone.utc).isoformat(),
    "producer": {
        "script": "truth_probe_runner.sh",
        "bead_id": "br-2k3qx.7.2",
    },
    "catalog_path": catalog_path,
    "catalog_id": catalog.get("catalog_id"),
    "schema_version": "truth_oracle_report.v1",
    "catalog_schema_version": catalog.get("schema_version"),
    "mode_runs": mode_runs,
    "surface_results": surface_results,
    "robot_family_results": robot_family_results,
    "stress_mode_results": stress_mode_results,
    "checks": checks_all,
    "mismatches": mismatches,
    "indeterminate_checks": indeterminate_checks,
    "culprit_surface_map": dict(culprit_surface_map),
    "summary": {
        "check_count": len(checks_all),
        "mismatch_count": mismatch_count,
        "indeterminate_count": indeterminate_count,
        "by_severity": severity_counts,
        "verdict": verdict,
    },
    "mismatch_count": mismatch_count,
    "indeterminate_count": indeterminate_count,
    "verdict": verdict,
}

report_path = os.path.join(output_dir, "truth_probe_report.json")
with open(report_path, "w", encoding="utf-8") as f:
    json.dump(report, f, indent=2, sort_keys=True)
PY

REPORT_PATH="${OUTPUT_DIR}/truth_probe_report.json"
[ -f "${REPORT_PATH}" ] || die "missing report: ${REPORT_PATH}"
MISMATCH_COUNT="$(json_get_top_key "${REPORT_PATH}" "mismatch_count" "0")"
INDETERMINATE_COUNT="$(json_get_top_key "${REPORT_PATH}" "indeterminate_count" "0")"
VERDICT="$(json_get_top_key "${REPORT_PATH}" "verdict" "UNKNOWN")"
require_int "MISMATCH_COUNT" "${MISMATCH_COUNT}"
require_int "INDETERMINATE_COUNT" "${INDETERMINATE_COUNT}"
OK_BOOL=true
if [ "${MISMATCH_COUNT}" -gt 0 ]; then
    OK_BOOL=false
elif [ "${INDETERMINATE_COUNT}" -gt 0 ]; then
    OK_BOOL=false
fi

printf '\n=== Truth Probe Summary ===\n' >&2
printf 'Output:        %s\n' "${OUTPUT_DIR}" >&2
printf 'Catalog:       %s\n' "${CATALOG_PATH}" >&2
printf 'Report:        %s\n' "${REPORT_PATH}" >&2
printf 'Mismatches:    %s\n' "${MISMATCH_COUNT}" >&2
printf 'Indeterminate: %s\n' "${INDETERMINATE_COUNT}" >&2
printf 'Verdict:       %s\n' "${VERDICT}" >&2
printf '===========================\n' >&2

python3 - "${OK_BOOL}" "${MODE}" "${CATALOG_PATH}" "${OUTPUT_DIR}" "${REPORT_PATH}" "${MISMATCH_COUNT}" "${INDETERMINATE_COUNT}" "${VERDICT}" <<'PY'
import json
import sys

ok = sys.argv[1].lower() == "true"
mode = sys.argv[2]
catalog_path = sys.argv[3]
output_dir = sys.argv[4]
report = sys.argv[5]
mismatch_count = int(sys.argv[6])
indeterminate_count = int(sys.argv[7])
verdict = sys.argv[8]

print(
    json.dumps(
        {
            "ok": ok,
            "bead_id": "br-2k3qx.7.2",
            "mode": mode,
            "catalog_path": catalog_path,
            "output_dir": output_dir,
            "report": report,
            "mismatch_count": mismatch_count,
            "indeterminate_count": indeterminate_count,
            "verdict": verdict,
        }
    )
)
PY

if [ "${MISMATCH_COUNT}" -gt 0 ] || [ "${INDETERMINATE_COUNT}" -gt 0 ]; then
    exit 2
fi
exit 0
