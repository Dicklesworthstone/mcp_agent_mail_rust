#!/usr/bin/env bash

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

BASELINE_PATH="${PROJECT_ROOT}/benches/atc_perf_baseline.json"
HISTORICAL_BASELINE_PATH="${PROJECT_ROOT}/tests/artifacts/perf/atc_pre_wiring_baseline.json"
OUTPUT_DIR="${PROJECT_ROOT}/tests/artifacts/perf/atc_perf_gate/$(date -u '+%Y%m%dT%H%M%SZ')"
THRESHOLD_PCT="5.0"
SKIP_BENCH=0
BENCH_REPORT_PATH=""
BENCH_TMPDIR_ROOT="${ATC_GATE_TMPDIR_ROOT:-/data/tmp}"
AM_BENCH_BIN="${AM_BENCH_BIN:-}"

usage() {
    cat <<'USAGE'
Usage: scripts/bench_atc_perf_gate.sh [options]

Runs the ATC send_message perf regression gate. The gate benchmarks the
mail-send hot path in three modes:
  - mail_send_no_atc      (ATC_LEARNING_DISABLED=1)
  - mail_send_atc_shadow  (AM_ATC_WRITE_MODE=shadow)
  - mail_send_atc_live    (AM_ATC_WRITE_MODE=live)

The gate enforces:
  1. a relative overhead cap where shadow/live p95 must stay within 5% of no_atc
  2. a 3-warmup / 50-measurement sample plan so p95 is not just "worst of 10"
  3. checked-in absolute baseline drift is reported for context, but it is advisory

Options:
  --baseline <path>             Checked-in baseline JSON for `am bench --baseline`
  --historical-baseline <path>  Optional checked-in artifact bundle copied into the output dir
  --output-dir <path>           Output directory for gate artifacts
  --threshold-pct <n>           Max allowed p95 overhead over no_atc (default: 5.0)
  --bench-report <path>         Reuse an existing `am bench` JSON report
  --skip-bench                  Skip running `cargo run ... am bench` and require --bench-report
Environment:
  AM_BENCH_BIN                  Use this prebuilt `am` binary instead of `cargo run`
  -h, --help                    Show this help text
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
        --historical-baseline)
            HISTORICAL_BASELINE_PATH="${2:-}"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="${2:-}"
            shift 2
            ;;
        --threshold-pct)
            THRESHOLD_PCT="${2:-}"
            shift 2
            ;;
        --bench-report)
            BENCH_REPORT_PATH="${2:-}"
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

mkdir -p "${OUTPUT_DIR}"
mkdir -p "${BENCH_TMPDIR_ROOT}"

if [ "${SKIP_BENCH}" -eq 1 ] && [ -z "${BENCH_REPORT_PATH}" ]; then
    printf 'error: --skip-bench requires --bench-report\n' >&2
    exit 2
fi

bench_log="${OUTPUT_DIR}/bench.log"

if [ "${SKIP_BENCH}" -eq 0 ]; then
    BENCH_REPORT_PATH="${OUTPUT_DIR}/bench_report.json"
    bench_runtime_root="$(mktemp -d "${BENCH_TMPDIR_ROOT}/atc-perf-gate.XXXXXX")"
    mkdir -p "${bench_runtime_root}/storage"
    bench_binary=""
    if [ -n "${AM_BENCH_BIN}" ] && [ -x "${AM_BENCH_BIN}" ]; then
        bench_binary="${AM_BENCH_BIN}"
    elif [ -n "${CARGO_TARGET_DIR:-}" ] && [ -x "${CARGO_TARGET_DIR}/debug/am" ]; then
        bench_binary="${CARGO_TARGET_DIR}/debug/am"
    elif [ -x "${PROJECT_ROOT}/target/debug/am" ]; then
        bench_binary="${PROJECT_ROOT}/target/debug/am"
    fi
    set +e
    (
        cd "${PROJECT_ROOT}"
        if [ -n "${bench_binary}" ]; then
            STORAGE_ROOT="${bench_runtime_root}/storage" \
            DATABASE_URL="sqlite:///${bench_runtime_root}/root.sqlite3" \
            "${bench_binary}" \
                bench \
                --json \
                --filter 'mail_send*' \
                --warmup 3 \
                --runs 50 \
                --baseline "${BASELINE_PATH}"
        else
            STORAGE_ROOT="${bench_runtime_root}/storage" \
            DATABASE_URL="sqlite:///${bench_runtime_root}/root.sqlite3" \
            cargo run -p mcp-agent-mail-cli --bin am -- \
                bench \
                --json \
                --filter 'mail_send*' \
                --warmup 3 \
                --runs 50 \
                --baseline "${BASELINE_PATH}"
        fi
    ) >"${BENCH_REPORT_PATH}" 2>"${bench_log}"
    bench_rc=$?
    set -e
else
    bench_rc=0
    cp "${BENCH_REPORT_PATH}" "${OUTPUT_DIR}/bench_report.json"
    BENCH_REPORT_PATH="${OUTPUT_DIR}/bench_report.json"
    printf 'skip-bench=1; reusing %s\n' "${BENCH_REPORT_PATH}" >"${bench_log}"
fi

if [ -f "${HISTORICAL_BASELINE_PATH}" ]; then
    cp "${HISTORICAL_BASELINE_PATH}" "${OUTPUT_DIR}/atc_pre_wiring_baseline.json"
fi

python3 - "${BASELINE_PATH}" "${BENCH_REPORT_PATH}" "${OUTPUT_DIR}" "${bench_rc}" "${THRESHOLD_PCT}" <<'PY'
import json
import pathlib
import sys

baseline_path = pathlib.Path(sys.argv[1])
bench_report_path = pathlib.Path(sys.argv[2])
output_dir = pathlib.Path(sys.argv[3])
bench_rc = int(sys.argv[4])
threshold_pct = float(sys.argv[5])

summary_path = output_dir / "summary.json"
comment_path = output_dir / "comment.md"

required = [
    "mail_send_no_atc",
    "mail_send_atc_shadow",
    "mail_send_atc_live",
]

def write_failure(status: str, reason: str) -> None:
    payload = {
        "schema_version": 1,
        "status": status,
        "reason": reason,
        "baseline_path": str(baseline_path),
        "bench_report_path": str(bench_report_path),
        "bench_exit_code": bench_rc,
        "threshold_pct": threshold_pct,
        "comparisons": [],
        "baseline_regressions": [],
    }
    summary_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
    comment_path.write_text(
        "\n".join(
            [
                "<!-- atc-perf-gate -->",
                "## ATC Perf Gate",
                "",
                f"Status: `{status}`",
                "",
                reason,
                "",
                f"- Baseline: `{baseline_path}`",
                f"- Bench report: `{bench_report_path}`",
            ]
        )
        + "\n",
        encoding="utf-8",
    )

if not baseline_path.is_file():
    write_failure("runtime_error", f"Missing baseline file: `{baseline_path}`.")
    sys.exit(1)

if not bench_report_path.is_file():
    write_failure("runtime_error", f"Missing bench report: `{bench_report_path}`.")
    sys.exit(1)

if bench_report_path.stat().st_size == 0:
    write_failure(
        "benchmark_failed",
        f"Bench report was empty: `{bench_report_path}`. Inspect bench.log for the cargo/am failure.",
    )
    sys.exit(1)

report = json.loads(bench_report_path.read_text(encoding="utf-8"))
benchmarks = report.get("summary", {}).get("benchmarks", {})
failures = report.get("failures", [])

missing = [name for name in required if name not in benchmarks]
if missing:
    write_failure(
        "runtime_error",
        "Missing ATC benchmark entries: " + ", ".join(f"`{name}`" for name in missing),
    )
    sys.exit(1)

no_atc = benchmarks["mail_send_no_atc"]
shadow = benchmarks["mail_send_atc_shadow"]
live = benchmarks["mail_send_atc_live"]

base_p95 = float(no_atc["p95_ms"])
allowed_p95 = round(base_p95 * (1.0 + threshold_pct / 100.0), 2)

comparisons = []
baseline_regressions = []
advisories = []

for name in required:
    entry = benchmarks[name]
    current_p95 = float(entry["p95_ms"])
    if entry.get("regression"):
        baseline_regressions.append(name)
    delta_ms = round(current_p95 - base_p95, 2)
    delta_pct = round(((current_p95 - base_p95) / base_p95) * 100.0, 2) if base_p95 else 0.0
    comparisons.append(
        {
            "name": name,
            "current_p95_ms": current_p95,
            "baseline_p95_ms": entry.get("baseline_p95_ms"),
            "baseline_delta_p95_ms": entry.get("delta_p95_ms"),
            "delta_vs_no_atc_ms": delta_ms,
            "delta_vs_no_atc_pct": delta_pct,
            "allowed_vs_no_atc_p95_ms": allowed_p95,
            "overhead_regression": name != "mail_send_no_atc" and current_p95 > allowed_p95,
            "absolute_regression": bool(entry.get("regression")),
        }
    )

status = "pass"
reason = ""
if failures:
    status = "benchmark_failed"
    reason = f"`am bench` reported {len(failures)} failure(s)."
elif bench_rc not in (0, 3):
    status = "benchmark_failed"
    reason = f"`am bench` exited with code {bench_rc}."

overhead_regressions = [c["name"] for c in comparisons if c["overhead_regression"]]
if baseline_regressions:
    advisories.append(
        "Absolute p95 drift versus the checked-in baseline was observed for: "
        + ", ".join(f"`{name}`" for name in baseline_regressions)
        + ". This is advisory only; the gate fails only when ATC overhead exceeds "
        f"{threshold_pct:.1f}% over `mail_send_no_atc`."
    )

if status == "pass" and overhead_regressions:
    status = "regression"
    reason = (
        "ATC send_message overhead exceeded the "
        f"{threshold_pct:.1f}% p95 budget for: "
        + ", ".join(f"`{name}`" for name in overhead_regressions)
    )

payload = {
    "schema_version": 1,
    "status": status,
    "reason": reason,
    "baseline_path": str(baseline_path),
    "bench_report_path": str(bench_report_path),
    "bench_exit_code": bench_rc,
    "threshold_pct": threshold_pct,
    "comparisons": comparisons,
    "baseline_regressions": baseline_regressions,
    "advisories": advisories,
    "failures": failures,
}
summary_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

status_line = {
    "pass": "PASS",
    "regression": "FAIL",
    "benchmark_failed": "ERROR",
    "runtime_error": "ERROR",
}.get(status, status.upper())

lines = [
    "<!-- atc-perf-gate -->",
    "## ATC Perf Gate",
    "",
    f"Status: `{status_line}`",
    "",
    f"- Baseline: `{baseline_path.name}`",
    f"- Relative overhead budget: `{threshold_pct:.1f}%` over `mail_send_no_atc` p95",
    f"- Bench report: `{bench_report_path.name}`",
    "",
    "| Benchmark | Current p95 | Baseline p95 | Delta vs no_atc | Allowed vs no_atc | Verdict |",
    "|---|---:|---:|---:|---:|---|",
]

for item in comparisons:
    verdict = "pass"
    if item["overhead_regression"]:
        verdict = "overhead regression"
    elif item["absolute_regression"]:
        verdict = "baseline drift (advisory)"
    lines.append(
        "| {name} | {current_p95_ms:.2f}ms | {baseline} | {delta:+.2f}% | {allowed:.2f}ms | {verdict} |".format(
            name=item["name"],
            current_p95_ms=item["current_p95_ms"],
            baseline=(
                f"{float(item['baseline_p95_ms']):.2f}ms"
                if item["baseline_p95_ms"] is not None
                else "-"
            ),
            delta=item["delta_vs_no_atc_pct"],
            allowed=item["allowed_vs_no_atc_p95_ms"],
            verdict=verdict,
        )
    )

if failures:
    lines.extend(
        [
            "",
            f"`am bench` failures: {len(failures)}",
        ]
    )
elif reason:
    lines.extend(["", reason])
elif advisories:
    lines.extend([""] + advisories)

comment_path.write_text("\n".join(lines) + "\n", encoding="utf-8")
PY

status="$(
    python3 - "${OUTPUT_DIR}/summary.json" <<'PY'
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
