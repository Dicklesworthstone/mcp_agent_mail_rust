#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

BASELINE_PATH="${PROJECT_ROOT}/benches/archive_perf_baseline.json"
SUMMARY_CSV_PATH="${PROJECT_ROOT}/tests/artifacts/perf/archive_batch_scaling.csv"
PROFILE_MD_PATH="${PROJECT_ROOT}/tests/artifacts/perf/archive_batch_100_profile.md"
BENCH_ROOT="${PROJECT_ROOT}"
RUN_ID="$(date -u '+%Y%m%dT%H%M%SZ')"
OUTPUT_DIR="${PROJECT_ROOT}/tests/artifacts/perf/archive_perf_gate/${RUN_ID}"
SKIP_BENCH=0
TOLERANCE_OVERRIDE=""

usage() {
    cat <<'USAGE'
Usage: scripts/bench_archive_perf_gate.sh [options]

Runs the archive batch benchmark perf gate, compares the current batch-{1,10,100}
warm-path results against the checked-in baseline, and writes machine-readable
and PR-friendly artifacts under tests/artifacts/perf/archive_perf_gate/<run_id>/.

Options:
  --baseline <path>      Checked-in baseline JSON
  --summary-csv <path>   Scaling CSV emitted by the archive benchmark harness
  --profile-md <path>    Markdown profile emitted by the archive benchmark harness
  --output-dir <path>    Output directory for gate artifacts
  --bench-root <path>    Cargo workspace root to benchmark (default: repo root)
  --tolerance-pct <n>    Override baseline tolerance percent from JSON
  --skip-bench           Reuse the existing CSV/profile artifacts without rerunning cargo bench
  -h, --help             Show help
USAGE
}

require_cmd() {
    local cmd="$1"
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        printf 'error: required command not found: %s\n' "${cmd}" >&2
        exit 2
    fi
}

while [ $# -gt 0 ]; do
    case "$1" in
        --baseline)
            BASELINE_PATH="${2:-}"
            shift 2
            ;;
        --summary-csv)
            SUMMARY_CSV_PATH="${2:-}"
            shift 2
            ;;
        --profile-md)
            PROFILE_MD_PATH="${2:-}"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="${2:-}"
            shift 2
            ;;
        --bench-root)
            BENCH_ROOT="${2:-}"
            shift 2
            ;;
        --tolerance-pct)
            TOLERANCE_OVERRIDE="${2:-}"
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

require_cmd cargo
require_cmd python3

BENCH_ROOT="$(cd "${BENCH_ROOT}" && pwd)"
mkdir -p "${OUTPUT_DIR}"

bench_log="${OUTPUT_DIR}/bench.log"

if [ "${SKIP_BENCH}" -eq 0 ]; then
    if [ ! -f "${BENCH_ROOT}/Cargo.toml" ]; then
        printf 'error: bench root does not look like a cargo workspace: %s\n' "${BENCH_ROOT}" >&2
        exit 2
    fi

    (
        cd "${BENCH_ROOT}"
        MCP_AGENT_MAIL_ARCHIVE_PROFILE=1 \
            cargo bench -p mcp-agent-mail --bench benchmarks -- archive_write_batch
    ) 2>&1 | tee "${bench_log}"
    bench_rc="${PIPESTATUS[0]}"
else
    printf 'skip-bench=1; reusing %s\n' "${SUMMARY_CSV_PATH}" | tee "${bench_log}"
    bench_rc=0
fi

if [ -f "${SUMMARY_CSV_PATH}" ]; then
    cp "${SUMMARY_CSV_PATH}" "${OUTPUT_DIR}/archive_batch_scaling.csv"
fi

if [ -f "${PROFILE_MD_PATH}" ]; then
    cp "${PROFILE_MD_PATH}" "${OUTPUT_DIR}/archive_batch_100_profile.md"
fi

python3 - "${BASELINE_PATH}" "${SUMMARY_CSV_PATH}" "${OUTPUT_DIR}" "${bench_rc}" "${TOLERANCE_OVERRIDE}" <<'PY'
import csv
import json
import pathlib
import sys
from typing import Any

baseline_path = pathlib.Path(sys.argv[1])
summary_csv_path = pathlib.Path(sys.argv[2])
output_dir = pathlib.Path(sys.argv[3])
bench_rc = int(sys.argv[4])
tolerance_override = sys.argv[5].strip()

summary_path = output_dir / "summary.json"
comment_path = output_dir / "comment.md"

def write_failure(status: str, reason: str) -> None:
    payload = {
        "schema_version": 1,
        "status": status,
        "reason": reason,
        "baseline_path": str(baseline_path),
        "summary_csv_path": str(summary_csv_path),
        "bench_exit_code": bench_rc,
        "regression_count": 0,
        "budget_breach_count": 0,
        "scenarios": [],
    }
    summary_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    comment_path.write_text(
        "\n".join(
            [
                "<!-- archive-perf-gate -->",
                "## Archive Perf Gate",
                "",
                f"Status: `{status}`",
                "",
                reason,
                "",
                f"- Baseline: `{baseline_path}`",
                f"- CSV: `{summary_csv_path}`",
            ]
        )
        + "\n",
        encoding="utf-8",
    )

if not baseline_path.is_file():
    write_failure("runtime_error", f"Missing baseline file: `{baseline_path}`.")
    sys.exit(1)

if not summary_csv_path.is_file():
    write_failure("runtime_error", f"Missing scaling CSV: `{summary_csv_path}`.")
    sys.exit(1)

baseline = json.loads(baseline_path.read_text(encoding="utf-8"))
tolerance_pct = float(tolerance_override or baseline.get("tolerance_pct", 10.0))
scenario_baselines: dict[str, Any] = baseline.get("scenarios", {})

current_rows: dict[str, dict[str, int]] = {}
with summary_csv_path.open("r", encoding="utf-8", newline="") as handle:
    reader = csv.DictReader(handle)
    for row in reader:
        batch_size = int(row["batch_size"])
        scenario_name = f"batch_no_attachments_{batch_size}"
        current_rows[scenario_name] = {
            "batch_size": batch_size,
            "p50_us": int(row["p50_us"]),
            "p95_us": int(row["p95_us"]),
            "p99_us": int(row["p99_us"]),
            "sample_count": int(row["sample_count"]),
        }

scenario_results = []
regression_count = 0
budget_breach_count = 0
status = "pass" if bench_rc == 0 else "benchmark_failed"

for scenario_name, expected in scenario_baselines.items():
    current = current_rows.get(scenario_name)
    if current is None:
        status = "runtime_error"
        scenario_results.append(
            {
                "scenario": scenario_name,
                "status": "missing_current_result",
                "message": f"scenario missing from {summary_csv_path}",
            }
        )
        continue

    allowed_p95_us = round(expected["baseline_p95_us"] * (1.0 + tolerance_pct / 100.0))
    allowed_p99_us = round(expected["baseline_p99_us"] * (1.0 + tolerance_pct / 100.0))
    delta_p95_us = current["p95_us"] - int(expected["baseline_p95_us"])
    delta_p99_us = current["p99_us"] - int(expected["baseline_p99_us"])
    delta_p95_pct = round((delta_p95_us / expected["baseline_p95_us"]) * 100.0, 2)
    delta_p99_pct = round((delta_p99_us / expected["baseline_p99_us"]) * 100.0, 2)

    p95_regression = current["p95_us"] > allowed_p95_us
    p99_regression = current["p99_us"] > allowed_p99_us
    budget_p95_breach = current["p95_us"] > int(expected["budget_p95_us"])
    budget_p99_breach = current["p99_us"] > int(expected["budget_p99_us"])

    if p95_regression or p99_regression:
        regression_count += 1
    if budget_p95_breach or budget_p99_breach:
        budget_breach_count += 1
    if (p95_regression or p99_regression or budget_p95_breach or budget_p99_breach) and status == "pass":
        status = "regression"

    scenario_status = "pass"
    if p95_regression or p99_regression:
        scenario_status = "regression"
    if budget_p95_breach or budget_p99_breach:
        scenario_status = "budget_breach"

    scenario_results.append(
        {
            "scenario": scenario_name,
            "status": scenario_status,
            "batch_size": current["batch_size"],
            "sample_count": current["sample_count"],
            "baseline_p50_us": int(expected["baseline_p50_us"]),
            "baseline_p95_us": int(expected["baseline_p95_us"]),
            "baseline_p99_us": int(expected["baseline_p99_us"]),
            "allowed_p95_us": allowed_p95_us,
            "allowed_p99_us": allowed_p99_us,
            "budget_p95_us": int(expected["budget_p95_us"]),
            "budget_p99_us": int(expected["budget_p99_us"]),
            "current_p50_us": current["p50_us"],
            "current_p95_us": current["p95_us"],
            "current_p99_us": current["p99_us"],
            "delta_p95_us": delta_p95_us,
            "delta_p95_pct": delta_p95_pct,
            "delta_p99_us": delta_p99_us,
            "delta_p99_pct": delta_p99_pct,
            "p95_regression": p95_regression,
            "p99_regression": p99_regression,
            "budget_p95_breach": budget_p95_breach,
            "budget_p99_breach": budget_p99_breach,
        }
    )

payload = {
    "schema_version": 1,
    "status": status,
    "baseline_path": str(baseline_path),
    "summary_csv_path": str(summary_csv_path),
    "bench_exit_code": bench_rc,
    "tolerance_pct": tolerance_pct,
    "source": baseline.get("source", {}),
    "regression_count": regression_count,
    "budget_breach_count": budget_breach_count,
    "scenarios": scenario_results,
}
summary_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

status_line = {
    "pass": "PASS",
    "regression": "FAIL",
    "benchmark_failed": "ERROR",
    "runtime_error": "ERROR",
}.get(status, status.upper())

lines = [
    "<!-- archive-perf-gate -->",
    "## Archive Perf Gate",
    "",
    f"Status: `{status_line}`",
    "",
    f"- Baseline: `{baseline_path.name}`",
    f"- Tolerance: `{tolerance_pct:.1f}%` over stored p95/p99 baselines",
    f"- Source CSV: `{summary_csv_path}`",
]

source = baseline.get("source", {})
source_commit = source.get("source_commit")
if source_commit:
    lines.append(f"- Baseline source commit: `{source_commit[:8]}`")

lines.extend(
    [
        "",
        "| Scenario | Baseline p95 | Current p95 | Delta | Budget p95 | Verdict |",
        "|---|---:|---:|---:|---:|---|",
    ]
)

for scenario in scenario_results:
    verdict = scenario.get("status", "unknown")
    if verdict == "pass":
        verdict = "pass"
    elif verdict == "budget_breach":
        verdict = "budget breach"
    else:
        verdict = "regression"
    lines.append(
        "| {scenario} | {baseline_p95_us}us | {current_p95_us}us | {delta_p95_pct:+.2f}% | {budget_p95_us}us | {verdict} |".format(
            **scenario,
            verdict=verdict,
        )
    )

if status != "pass":
    lines.extend(
        [
            "",
            "Apply the `perf-regression-acknowledged` label only for an intentional, reviewed perf tradeoff. Runtime/benchmark failures still fail the workflow.",
        ]
    )

comment_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
PY

status="$(python3 - "${OUTPUT_DIR}/summary.json" <<'PY'
import json
import pathlib
import sys

summary = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
print(summary["status"])
PY
)"

case "${status}" in
    pass)
        exit 0
        ;;
    regression)
        exit 3
        ;;
    *)
        exit 1
        ;;
esac
