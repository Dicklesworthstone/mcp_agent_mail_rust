#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

PHASE="${1:-}"
if [ -n "${PHASE}" ]; then
    shift
fi

DATE="$(date -u '+%Y%m%d')"
OUTPUT_PATH=""
SOURCE_SUMMARY=""
SOURCE_SCALING_CSV="${PROJECT_ROOT}/tests/artifacts/perf/archive_batch_scaling.csv"
SOURCE_STRESS_JSON=""
SKIP_BENCH=0

usage() {
    cat <<'USAGE'
Usage: scripts/perf/capture_archive_baseline.sh <pre|post> [options]

Captures an archive-write baseline JSON artifact for br-q8yaa.

Options:
  --output <path>              Output JSON path. Defaults to tests/artifacts/perf/baseline_<phase>_fix_<date>.json
  --date <yyyymmdd>            Date stamp for the default output path
  --source-summary <path>      Archive harness summary.json with single-message samples
  --source-scaling-csv <path>  Archive batch scaling CSV with batch 1/10/100/1000 samples
  --source-stress-json <path>  30-agent stress JSON summary
  --skip-bench                 Reuse existing source artifacts instead of running cargo bench
  -h, --help                   Show this help

Agent workflow note: when this script runs a benchmark, invoke it through rch
from agent sessions, for example:
  rch exec -- bash scripts/perf/capture_archive_baseline.sh post
USAGE
}

if [ "${PHASE}" = "-h" ] || [ "${PHASE}" = "--help" ]; then
    usage
    exit 0
fi

case "${PHASE}" in
    pre|post) ;;
    *)
        printf 'error: phase must be `pre` or `post`\n' >&2
        usage >&2
        exit 2
        ;;
esac

while [ $# -gt 0 ]; do
    case "$1" in
        --output)
            OUTPUT_PATH="${2:-}"
            shift 2
            ;;
        --date)
            DATE="${2:-}"
            shift 2
            ;;
        --source-summary)
            SOURCE_SUMMARY="${2:-}"
            shift 2
            ;;
        --source-scaling-csv)
            SOURCE_SCALING_CSV="${2:-}"
            shift 2
            ;;
        --source-stress-json)
            SOURCE_STRESS_JSON="${2:-}"
            shift 2
            ;;
        --skip-bench)
            SKIP_BENCH=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            printf 'error: unknown argument: %s\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

if [ -z "${OUTPUT_PATH}" ]; then
    OUTPUT_PATH="${PROJECT_ROOT}/tests/artifacts/perf/baseline_${PHASE}_fix_${DATE}.json"
fi

if [ "${SKIP_BENCH}" -eq 0 ]; then
    (
        cd "${PROJECT_ROOT}"
        MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 \
            MCP_AGENT_MAIL_BENCH_SCOPE=archive_write \
            cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch
    )
fi

if [ -z "${SOURCE_SUMMARY}" ]; then
    if [ -d "${PROJECT_ROOT}/tests/artifacts/bench/archive" ]; then
        SOURCE_SUMMARY="$(
            find "${PROJECT_ROOT}/tests/artifacts/bench/archive" \
                -maxdepth 3 \
                -name summary.json \
                -printf '%T@ %p\n' 2>/dev/null \
                | sort -nr \
                | awk 'NR == 1 { print substr($0, index($0, $2)) }'
        )"
    fi
fi

mkdir -p "$(dirname "${OUTPUT_PATH}")"

python3 - \
    "${PHASE}" \
    "${DATE}" \
    "${OUTPUT_PATH}" \
    "${SOURCE_SUMMARY}" \
    "${SOURCE_SCALING_CSV}" \
    "${SOURCE_STRESS_JSON}" <<'PY'
import csv
import json
import os
import pathlib
import platform
import socket
import subprocess
import sys
from typing import Any

phase = sys.argv[1]
date = sys.argv[2]
output_path = pathlib.Path(sys.argv[3])
summary_path = pathlib.Path(sys.argv[4]) if sys.argv[4] else None
scaling_csv_path = pathlib.Path(sys.argv[5])
stress_json_path = pathlib.Path(sys.argv[6]) if sys.argv[6] else None

MANDATED_POINTS = [
    "batch-1",
    "batch-10",
    "batch-100",
    "batch-1000",
    "single-attachment",
    "30-agent-stress",
]


def command_output(command: list[str]) -> str:
    try:
        return subprocess.check_output(command, text=True, stderr=subprocess.DEVNULL).strip()
    except (OSError, subprocess.CalledProcessError):
        return ""


def mem_total_gb() -> int:
    try:
        for line in pathlib.Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
            if line.startswith("MemTotal:"):
                kb = int(line.split()[1])
                return round(kb / 1024 / 1024)
    except (OSError, ValueError):
        return 0
    return 0


def cpu_model() -> str:
    try:
        for line in pathlib.Path("/proc/cpuinfo").read_text(encoding="utf-8").splitlines():
            if line.startswith("model name"):
                return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or "unknown"


def read_summary_results(path: pathlib.Path | None) -> dict[str, dict[str, Any]]:
    if path is None or not path.is_file():
        return {}
    payload = json.loads(path.read_text(encoding="utf-8"))
    return {
        str(item["scenario"]): item
        for item in payload.get("results", [])
        if isinstance(item, dict) and "scenario" in item
    }


def read_scaling_rows(path: pathlib.Path) -> dict[str, dict[str, Any]]:
    if not path.is_file():
        return {}
    rows = {}
    with path.open("r", encoding="utf-8", newline="") as handle:
        for row in csv.DictReader(handle):
            batch_size = int(row["batch_size"])
            rows[f"batch-{batch_size}"] = {
                "status": "measured",
                "source": str(path),
                "scenario": f"batch_no_attachments_{batch_size}",
                "sample_count": int(row["sample_count"]),
                "elements_per_op": batch_size,
                "p50_us": int(row["p50_us"]),
                "p95_us": int(row["p95_us"]),
                "p99_us": int(row["p99_us"]),
                "p99_9_us": int(row.get("p99_9_us") or row["p99_us"]),
                "p99_99_us": int(row.get("p99_99_us") or row.get("p99_9_us") or row["p99_us"]),
                "max_us": int(row.get("max_us") or row.get("p99_99_us") or row["p99_us"]),
                "throughput_elements_per_sec": float(row.get("throughput_elements_per_sec") or 0.0),
            }
    return rows


def scenario_point(
    name: str,
    scenario: str,
    summary_results: dict[str, dict[str, Any]],
    source: pathlib.Path | None,
) -> dict[str, Any] | None:
    item = summary_results.get(scenario)
    if not item:
        return None
    return {
        "status": "measured",
        "source": str(source) if source is not None else "",
        "scenario": scenario,
        "sample_count": len(item.get("samples_us", [])),
        "elements_per_op": int(item.get("elements_per_op", 1)),
        "p50_us": int(item["p50_us"]),
        "p95_us": int(item["p95_us"]),
        "p99_us": int(item["p99_us"]),
        "p99_9_us": int(item.get("p99_9_us", item["p99_us"])),
        "p99_99_us": int(item.get("p99_99_us", item.get("p99_9_us", item["p99_us"]))),
        "max_us": int(item.get("max_us", item.get("p99_us", 0))),
        "budget_p95_us": int(item.get("budget_p95_us", 0)),
        "budget_p99_us": int(item.get("budget_p99_us", 0)),
        "throughput_elements_per_sec": float(item.get("throughput_elements_per_sec", 0.0)),
    }


def stress_point(path: pathlib.Path | None) -> dict[str, Any] | None:
    if path is None or not path.is_file():
        return None
    payload = json.loads(path.read_text(encoding="utf-8"))
    metrics = payload.get("metrics", payload)
    return {
        "status": str(payload.get("status", "measured")),
        "source": str(path),
        "scenario": "30-agent-stress",
        "agent_count": int(metrics.get("agent_count", metrics.get("agents", 30))),
        "message_count": int(metrics.get("message_count", metrics.get("messages", 0))),
        "sample_count": int(metrics.get("sample_count", 1)),
        "p50_us": int(metrics.get("p50_us", metrics.get("p50_ms", 0) * 1000)),
        "p95_us": int(metrics.get("p95_us", metrics.get("p95_ms", 0) * 1000)),
        "p99_us": int(metrics.get("p99_us", metrics.get("p99_ms", 0) * 1000)),
        "max_us": int(metrics.get("max_us", metrics.get("max_ms", 0) * 1000)),
        "throughput_elements_per_sec": float(metrics.get("throughput_elements_per_sec", 0.0)),
    }


summary_results = read_summary_results(summary_path)
scaling_points = read_scaling_rows(scaling_csv_path)

bench_points: dict[str, dict[str, Any]] = {}
for point in ["batch-1", "batch-10", "batch-100", "batch-1000"]:
    if point in scaling_points:
        bench_points[point] = scaling_points[point]

single_attachment = scenario_point(
    "single-attachment",
    "single_inline_attachment",
    summary_results,
    summary_path,
)
if single_attachment is not None:
    bench_points["single-attachment"] = single_attachment

stress = stress_point(stress_json_path)
if stress is not None:
    bench_points["30-agent-stress"] = stress

missing_points = [name for name in MANDATED_POINTS if name not in bench_points]

payload = {
    "schema_version": 1,
    "bead_id": "br-q8yaa",
    "status": "complete" if not missing_points else "incomplete",
    "missing_points": missing_points,
    "meta": {
        "phase": phase,
        "date": date,
        "host": socket.gethostname(),
        "kernel": platform.release(),
        "cpu_model": cpu_model(),
        "mem_total_gb": mem_total_gb(),
        "rustc_version": command_output(["rustc", "--version"]),
        "cargo_version": command_output(["cargo", "--version"]),
        "profile": "release",
        "source_summary": str(summary_path) if summary_path else "",
        "source_scaling_csv": str(scaling_csv_path),
        "source_stress_json": str(stress_json_path) if stress_json_path else "",
    },
    "bench_points": bench_points,
}

output_path.write_text(json.dumps(payload, indent=2, sort_keys=True) + "\n", encoding="utf-8")

if missing_points:
    print(
        "error: baseline artifact is missing mandated point(s): "
        + ", ".join(missing_points),
        file=sys.stderr,
    )
    sys.exit(1)

print(output_path)
PY
