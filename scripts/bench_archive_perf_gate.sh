#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

BASELINE_PATH="${PROJECT_ROOT}/benches/archive_perf_baseline.json"
SUMMARY_CSV_PATH="${PROJECT_ROOT}/tests/artifacts/perf/archive_batch_scaling.csv"
PROFILE_MD_PATH="${PROJECT_ROOT}/tests/artifacts/perf/archive_batch_100_profile.md"
SPANS_JSON_PATH="${PROJECT_ROOT}/tests/artifacts/perf/archive_batch_100_spans.json"
BENCH_ROOT="${PROJECT_ROOT}"
RUN_ID="$(date -u '+%Y%m%dT%H%M%SZ')"
OUTPUT_DIR="${PROJECT_ROOT}/tests/artifacts/perf/archive_perf_gate/${RUN_ID}"
SKIP_BENCH=0
TOLERANCE_OVERRIDE=""
STARTUP_SAMPLES=5

usage() {
    cat <<'USAGE'
Usage: scripts/bench_archive_perf_gate.sh [options]

Runs the archive batch benchmark perf gate, compares the current batch-{1,10,100}
warm-path results plus cold-start / peak-RSS probes against the checked-in
baseline, and writes machine-readable and PR-friendly artifacts under
tests/artifacts/perf/archive_perf_gate/<run_id>/.

Options:
  --baseline <path>      Checked-in baseline JSON
  --summary-csv <path>   Scaling CSV emitted by the archive benchmark harness
  --profile-md <path>    Markdown profile emitted by the archive benchmark harness
  --spans-json <path>    JSON profile emitted by the archive benchmark harness
  --output-dir <path>    Output directory for gate artifacts
  --bench-root <path>    Cargo workspace root to benchmark (default: repo root)
  --tolerance-pct <n>    Override baseline tolerance percent from JSON
  --startup-samples <n>  Number of cold-start samples for `am serve-http`
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
        --spans-json)
            SPANS_JSON_PATH="${2:-}"
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
        --startup-samples)
            STARTUP_SAMPLES="${2:-}"
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

if [ -f "${SPANS_JSON_PATH}" ]; then
    cp "${SPANS_JSON_PATH}" "${OUTPUT_DIR}/archive_batch_100_spans.json"
fi

startup_binary=""
if [ -x "${BENCH_ROOT}/target/release/am" ]; then
    startup_binary="${BENCH_ROOT}/target/release/am"
elif [ -x "${BENCH_ROOT}/target/debug/am" ]; then
    startup_binary="${BENCH_ROOT}/target/debug/am"
else
    set +e
    (
        cd "${BENCH_ROOT}"
        cargo build -p mcp-agent-mail-cli --release
    ) >> "${bench_log}" 2>&1
    build_rc=$?
    set -e
    if [ "${build_rc}" -eq 0 ] && [ -x "${BENCH_ROOT}/target/release/am" ]; then
        startup_binary="${BENCH_ROOT}/target/release/am"
    fi
fi

startup_probe_path="${OUTPUT_DIR}/startup_probe.json"
startup_probe_rc=0

if [ -n "${startup_binary}" ]; then
    set +e
    python3 - "${startup_binary}" "${startup_probe_path}" "${STARTUP_SAMPLES}" <<'PY'
import json
import os
import pathlib
import signal
import socket
import subprocess
import sys
import tempfile
import time

binary_path = pathlib.Path(sys.argv[1]).resolve()
output_path = pathlib.Path(sys.argv[2])
sample_count = max(1, int(sys.argv[3]))
repo_root = binary_path.parent.parent.parent
warmup_runs = 1

def percentile(values, pct):
    if not values:
        return 0
    ordered = sorted(values)
    idx = round((len(ordered) - 1) * pct)
    return ordered[idx]

def pick_port():
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port

samples_ms = []
peak_rss_samples_kb = []
peak_hwm_samples_kb = []

for run_index in range(sample_count + warmup_runs):
    root = pathlib.Path(tempfile.mkdtemp(prefix="archive-startup-probe-"))
    port = pick_port()
    env = os.environ.copy()
    env["AM_INTERFACE_MODE"] = "cli"
    env["DATABASE_URL"] = f"sqlite:///{root / 'startup.sqlite3'}"
    env["STORAGE_ROOT"] = str(root / "archive")

    proc = subprocess.Popen(
        [
            str(binary_path),
            "serve-http",
            "--host",
            "127.0.0.1",
            "--port",
            str(port),
            "--path",
            "mcp",
            "--no-auth",
            "--no-tui",
        ],
        cwd=str(repo_root),
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )

    start = time.time()
    peak_rss_kb = 0
    peak_hwm_kb = 0
    status_path = pathlib.Path(f"/proc/{proc.pid}/status")
    ready = False

    while time.time() - start < 10:
        if status_path.exists():
            try:
                for line in status_path.read_text(encoding="utf-8").splitlines():
                    if line.startswith("VmRSS:"):
                        peak_rss_kb = max(peak_rss_kb, int(line.split()[1]))
                    elif line.startswith("VmHWM:"):
                        peak_hwm_kb = max(peak_hwm_kb, int(line.split()[1]))
            except OSError:
                pass
        try:
            sock = socket.create_connection(("127.0.0.1", port), timeout=0.05)
            sock.close()
            ready = True
            break
        except OSError:
            if proc.poll() is not None:
                break
            time.sleep(0.01)

    elapsed_ms = int((time.time() - start) * 1000)

    proc.send_signal(signal.SIGINT)
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()

    if not ready:
        output_path.write_text(
            json.dumps(
                {
                    "schema_version": 1,
                    "status": "runtime_error",
                    "reason": "startup probe never observed a listening socket",
                    "binary_path": str(binary_path),
                },
                indent=2,
            )
            + "\n",
            encoding="utf-8",
        )
        raise SystemExit(1)

    if run_index < warmup_runs:
        continue

    samples_ms.append(elapsed_ms)
    peak_rss_samples_kb.append(peak_rss_kb)
    peak_hwm_samples_kb.append(max(peak_hwm_kb, peak_rss_kb))

payload = {
    "schema_version": 1,
    "status": "pass",
    "binary_path": str(binary_path),
    "sample_count": sample_count,
    "warmup_runs_discarded": warmup_runs,
    "samples_ms": samples_ms,
    "p50_ms": percentile(samples_ms, 0.50),
    "p95_ms": percentile(samples_ms, 0.95),
    "peak_rss_kb": max(peak_rss_samples_kb) if peak_rss_samples_kb else 0,
    "peak_hwm_kb": max(peak_hwm_samples_kb) if peak_hwm_samples_kb else 0,
}
output_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY
    startup_probe_rc=$?
    set -e
else
    startup_probe_rc=2
fi

python3 - "${BASELINE_PATH}" "${SUMMARY_CSV_PATH}" "${OUTPUT_DIR}" "${bench_rc}" "${TOLERANCE_OVERRIDE}" "${startup_probe_path}" "${startup_probe_rc}" <<'PY'
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
startup_probe_path = pathlib.Path(sys.argv[6])
startup_probe_rc = int(sys.argv[7])

summary_path = output_dir / "summary.json"
comment_path = output_dir / "comment.md"
extended_dimensions_path = output_dir / "extended_dimensions.json"

def write_failure(status: str, reason: str) -> None:
    payload = {
        "schema_version": 2,
        "status": status,
        "reason": reason,
        "baseline_path": str(baseline_path),
        "summary_csv_path": str(summary_csv_path),
        "bench_exit_code": bench_rc,
        "regression_count": 0,
        "budget_breach_count": 0,
        "scenarios": [],
        "extended": {},
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
extended_baseline: dict[str, Any] = baseline.get("extended", {})

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
            "p99_9_us": int(row.get("p99_9_us") or row["p99_us"]),
            "p99_99_us": int(row.get("p99_99_us") or row.get("p99_9_us") or row["p99_us"]),
            "max_us": int(row.get("max_us") or row.get("p99_99_us") or row["p99_us"]),
            "sample_count": int(row["sample_count"]),
        }

scenario_results = []
tail_results = []
regression_count = 0
budget_breach_count = 0
status = "pass" if bench_rc == 0 else "benchmark_failed"

tail_defaults = extended_baseline.get("tail", {})
tail_regression_pct = float(tail_defaults.get("tail_regression_pct", 50.0))
tail_multiplier = 1.0 + (tail_regression_pct / 100.0)
tail_ratio_limit = float(tail_defaults.get("max_tail_ratio", 5.0))

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
    baseline_p99_9_us = int(expected.get("baseline_p99_9_us", expected["baseline_p99_us"]))
    baseline_p99_99_us = int(expected.get("baseline_p99_99_us", expected["baseline_p99_us"]))
    allowed_p99_9_us = int(expected.get("allowed_p99_9_us", round(baseline_p99_9_us * tail_multiplier)))
    allowed_p99_99_us = int(expected.get("allowed_p99_99_us", round(baseline_p99_99_us * tail_multiplier)))
    delta_p95_us = current["p95_us"] - int(expected["baseline_p95_us"])
    delta_p99_us = current["p99_us"] - int(expected["baseline_p99_us"])
    delta_p95_pct = round((delta_p95_us / expected["baseline_p95_us"]) * 100.0, 2)
    delta_p99_pct = round((delta_p99_us / expected["baseline_p99_us"]) * 100.0, 2)
    delta_p99_9_us = current["p99_9_us"] - baseline_p99_9_us
    delta_p99_99_us = current["p99_99_us"] - baseline_p99_99_us
    delta_p99_9_pct = round((delta_p99_9_us / baseline_p99_9_us) * 100.0, 2) if baseline_p99_9_us else 0.0
    delta_p99_99_pct = round((delta_p99_99_us / baseline_p99_99_us) * 100.0, 2) if baseline_p99_99_us else 0.0

    p95_regression = current["p95_us"] > allowed_p95_us
    p99_regression = current["p99_us"] > allowed_p99_us
    p99_9_regression = current["p99_9_us"] > allowed_p99_9_us
    p99_99_regression = current["p99_99_us"] > allowed_p99_99_us
    budget_p95_breach = current["p95_us"] > int(expected["budget_p95_us"])
    budget_p99_breach = current["p99_us"] > int(expected["budget_p99_us"])
    tail_budget_us = int(expected["budget_p95_us"] * tail_ratio_limit)
    tail_ratio_breach = current["p99_9_us"] > tail_budget_us

    if p95_regression or p99_regression or p99_9_regression or p99_99_regression:
        regression_count += 1
    if budget_p95_breach or budget_p99_breach or tail_ratio_breach:
        budget_breach_count += 1
    if (
        p95_regression
        or p99_regression
        or p99_9_regression
        or p99_99_regression
        or budget_p95_breach
        or budget_p99_breach
        or tail_ratio_breach
    ) and status == "pass":
        status = "regression"

    scenario_status = "pass"
    if p95_regression or p99_regression or p99_9_regression or p99_99_regression:
        scenario_status = "regression"
    if budget_p95_breach or budget_p99_breach or tail_ratio_breach:
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
    tail_results.append(
        {
            "scenario": scenario_name,
            "status": scenario_status if (p99_9_regression or p99_99_regression or tail_ratio_breach) else "pass",
            "sample_count": current["sample_count"],
            "baseline_p99_9_us": baseline_p99_9_us,
            "baseline_p99_99_us": baseline_p99_99_us,
            "allowed_p99_9_us": allowed_p99_9_us,
            "allowed_p99_99_us": allowed_p99_99_us,
            "current_p99_9_us": current["p99_9_us"],
            "current_p99_99_us": current["p99_99_us"],
            "current_max_us": current["max_us"],
            "delta_p99_9_us": delta_p99_9_us,
            "delta_p99_9_pct": delta_p99_9_pct,
            "delta_p99_99_us": delta_p99_99_us,
            "delta_p99_99_pct": delta_p99_99_pct,
            "tail_budget_us": tail_budget_us,
            "tail_ratio_limit": tail_ratio_limit,
            "current_tail_ratio": round(current["p99_9_us"] / expected["budget_p95_us"], 3) if expected["budget_p95_us"] else 0.0,
            "p99_9_regression": p99_9_regression,
            "p99_99_regression": p99_99_regression,
            "tail_ratio_breach": tail_ratio_breach,
        }
    )

startup_result = {
    "status": "runtime_error",
    "reason": "startup probe did not run",
}

if startup_probe_rc == 0 and startup_probe_path.is_file():
    startup_probe = json.loads(startup_probe_path.read_text(encoding="utf-8"))
    if startup_probe.get("status") == "pass":
        startup_baseline = extended_baseline.get("startup", {})
        memory_baseline = extended_baseline.get("memory", {})
        baseline_startup_p95_ms = int(startup_baseline.get("baseline_p95_ms", 0))
        allowed_startup_p95_ms = int(startup_baseline.get("allowed_p95_ms", baseline_startup_p95_ms))
        budget_startup_p95_ms = int(startup_baseline.get("budget_p95_ms", allowed_startup_p95_ms))
        baseline_peak_rss_kb = int(memory_baseline.get("baseline_peak_rss_kb", 0))
        allowed_peak_rss_kb = int(memory_baseline.get("allowed_peak_rss_kb", baseline_peak_rss_kb))
        budget_peak_rss_kb = int(memory_baseline.get("budget_peak_rss_kb", allowed_peak_rss_kb))

        current_startup_p95_ms = int(startup_probe["p95_ms"])
        current_peak_rss_kb = int(startup_probe.get("peak_hwm_kb") or startup_probe.get("peak_rss_kb") or 0)
        startup_regression = current_startup_p95_ms > allowed_startup_p95_ms
        startup_budget_breach = current_startup_p95_ms > budget_startup_p95_ms
        memory_regression = current_peak_rss_kb > allowed_peak_rss_kb
        memory_budget_breach = current_peak_rss_kb > budget_peak_rss_kb

        if startup_regression or memory_regression:
            regression_count += 1
        if startup_budget_breach or memory_budget_breach:
            budget_breach_count += 1
        if (startup_regression or startup_budget_breach or memory_regression or memory_budget_breach) and status == "pass":
            status = "regression"

        startup_result = {
            "status": (
                "budget_breach"
                if startup_budget_breach or memory_budget_breach
                else "regression"
                if startup_regression or memory_regression
                else "pass"
            ),
            "binary_path": startup_probe["binary_path"],
            "sample_count": int(startup_probe["sample_count"]),
            "samples_ms": startup_probe["samples_ms"],
            "current_p50_ms": int(startup_probe["p50_ms"]),
            "current_p95_ms": current_startup_p95_ms,
            "baseline_p95_ms": baseline_startup_p95_ms,
            "allowed_p95_ms": allowed_startup_p95_ms,
            "budget_p95_ms": budget_startup_p95_ms,
            "delta_p95_ms": current_startup_p95_ms - baseline_startup_p95_ms,
            "delta_p95_pct": round(((current_startup_p95_ms - baseline_startup_p95_ms) / baseline_startup_p95_ms) * 100.0, 2) if baseline_startup_p95_ms else 0.0,
            "current_peak_rss_kb": current_peak_rss_kb,
            "baseline_peak_rss_kb": baseline_peak_rss_kb,
            "allowed_peak_rss_kb": allowed_peak_rss_kb,
            "budget_peak_rss_kb": budget_peak_rss_kb,
            "delta_peak_rss_kb": current_peak_rss_kb - baseline_peak_rss_kb,
            "delta_peak_rss_pct": round(((current_peak_rss_kb - baseline_peak_rss_kb) / baseline_peak_rss_kb) * 100.0, 2) if baseline_peak_rss_kb else 0.0,
            "startup_regression": startup_regression,
            "startup_budget_breach": startup_budget_breach,
            "memory_regression": memory_regression,
            "memory_budget_breach": memory_budget_breach,
        }
    else:
        startup_result = startup_probe
        if status == "pass":
            status = "runtime_error"
elif status == "pass":
    status = "runtime_error"

extended_payload = {
    "schema_version": 1,
    "startup": startup_result,
    "tail": {
        "tail_regression_pct": tail_regression_pct,
        "max_tail_ratio": tail_ratio_limit,
        "sample_note": "Current warm-path sample counts are below 1,000 per scenario, so p99.9/p99.99 collapse to the worst observed sample and act as a conservative tail sentinel.",
        "scenarios": tail_results,
    },
    "verdict": {
        "all_dimensions_pass": status == "pass",
        "regressions": [
            item["scenario"]
            for item in tail_results
            if item.get("status") != "pass"
        ]
        + ([] if startup_result.get("status") == "pass" else ["startup_or_memory"]),
    },
}
extended_dimensions_path.write_text(json.dumps(extended_payload, indent=2) + "\n", encoding="utf-8")

payload = {
    "schema_version": 2,
    "status": status,
    "baseline_path": str(baseline_path),
    "summary_csv_path": str(summary_csv_path),
    "extended_dimensions_path": str(extended_dimensions_path),
    "bench_exit_code": bench_rc,
    "tolerance_pct": tolerance_pct,
    "source": baseline.get("source", {}),
    "regression_count": regression_count,
    "budget_breach_count": budget_breach_count,
    "scenarios": scenario_results,
    "extended": extended_payload,
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
    f"- Extended report: `{extended_dimensions_path}`",
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

lines.extend(
    [
        "",
        "| Scenario | Baseline p99.9 | Current p99.9 | Delta | Tail budget | Verdict |",
        "|---|---:|---:|---:|---:|---|",
    ]
)

for scenario in tail_results:
    verdict = scenario.get("status", "unknown")
    if verdict == "pass":
        verdict = "pass"
    elif verdict == "budget_breach":
        verdict = "tail budget breach"
    else:
        verdict = "tail regression"
    lines.append(
        "| {scenario} | {baseline_p99_9_us}us | {current_p99_9_us}us | {delta_p99_9_pct:+.2f}% | {tail_budget_us}us | {verdict} |".format(
            **scenario,
            verdict=verdict,
        )
    )

if startup_result.get("status") != "runtime_error":
    startup_verdict = startup_result["status"].replace("_", " ")
    lines.extend(
        [
            "",
            "| Dimension | Baseline | Current | Allowed | Budget | Verdict |",
            "|---|---:|---:|---:|---:|---|",
            "| serve-http startup p95 | {baseline_p95_ms}ms | {current_p95_ms}ms | {allowed_p95_ms}ms | {budget_p95_ms}ms | {startup_verdict} |".format(
                **startup_result,
                startup_verdict=startup_verdict,
            ),
            "| serve-http peak RSS | {baseline_peak_rss_kb}KB | {current_peak_rss_kb}KB | {allowed_peak_rss_kb}KB | {budget_peak_rss_kb}KB | {startup_verdict} |".format(
                **startup_result,
                startup_verdict=startup_verdict,
            ),
        ]
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
