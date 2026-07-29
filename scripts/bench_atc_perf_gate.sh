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
CANARY_DB_PATH=""
CANARY_DB_EXPLICIT=0
DEFAULT_CANARY_STATE_ROOT="${STORAGE_ROOT:-${HOME:-}/.mcp_agent_mail_git_mailbox_repo}"
CANARY_REPORT_STATE_DIR="${AM_ATC_CANARY_REPORT_DIR:-${DEFAULT_CANARY_STATE_ROOT}/atc_perf_gate}"

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
  --canary-db <path>            Optional existing SQLite DB for quick_check + ATC row count
  --skip-bench                  Skip running `cargo run ... am bench` and require --bench-report
Environment:
  AM_BENCH_BIN                  Use this prebuilt `am` binary instead of `cargo run`
  AM_ATC_CANARY_REPORT_DIR      Directory for latest_canary_report.json operator surface
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

write_server_canary_status() {
    local status_path="$1"
    local status="$2"
    local reason="$3"
    local stdout_path="$4"
    local stderr_path="$5"
    local project_path="$6"

    python3 - "${status_path}" "${status}" "${reason}" "${stdout_path}" "${stderr_path}" "${project_path}" <<'PY'
import json
import pathlib
import sys

status_path = pathlib.Path(sys.argv[1])
payload = {
    "status": sys.argv[2],
    "reason": sys.argv[3],
    "stdout": sys.argv[4],
    "stderr": sys.argv[5],
    "project": sys.argv[6],
}
status_path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY
}

run_server_canary() {
    local db_path="$1"
    local storage_root="$2"
    local output_dir="$3"
    local server_binary="$4"
    local status_path="$5"

    local project_path="/tmp/am-atc-perf-canary"
    local fifo="${output_dir}/server_canary.stdin.$$"
    local stdout_path="${output_dir}/server_canary_stdio.jsonl"
    local stderr_path="${output_dir}/server_canary_stderr.log"
    local writer_rc=0
    local server_rc=0
    local status="completed"
    local reason="server stdio canary completed"

    mkfifo "${fifo}"

    set +e
    if [ -n "${server_binary}" ]; then
        DATABASE_URL="sqlite:///${db_path}" \
        STORAGE_ROOT="${storage_root}" \
        AM_ATC_WRITE_MODE=live \
        TUI_ENABLED=false \
        RUST_LOG=error \
        "${server_binary}" serve-stdio <"${fifo}" >"${stdout_path}" 2>"${stderr_path}" &
    else
        (
            cd "${PROJECT_ROOT}" &&
            DATABASE_URL="sqlite:///${db_path}" \
            STORAGE_ROOT="${storage_root}" \
            AM_ATC_WRITE_MODE=live \
            TUI_ENABLED=false \
            RUST_LOG=error \
            cargo run -p mcp-agent-mail-cli --bin am -- serve-stdio
        ) <"${fifo}" >"${stdout_path}" 2>"${stderr_path}" &
    fi
    local server_pid=$!

    {
        printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"atc-perf-gate","version":"1"}}}'
        sleep 0.5
        printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"ensure_project","arguments":{"human_key":"/tmp/am-atc-perf-canary"}}}'
        sleep 0.4
        printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/am-atc-perf-canary","name":"BlueLake","program":"bench","model":"bench","task_description":"server atc canary"}}}'
        sleep 0.4
        printf '%s\n' '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"register_agent","arguments":{"project_key":"/tmp/am-atc-perf-canary","name":"RedFox","program":"bench","model":"bench","task_description":"server atc canary"}}}'
        sleep 0.4
        printf '%s\n' '{"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"send_message","arguments":{"project_key":"/tmp/am-atc-perf-canary","sender_name":"BlueLake","to":["RedFox"],"subject":"ATC perf gate server canary","body_md":"ATC perf gate server canary","thread_id":"ATC-CANARY"}}}'
        sleep 2
    } >"${fifo}" &
    local writer_pid=$!

    wait "${writer_pid}"
    writer_rc=$?
    sleep 2
    kill "${server_pid}" 2>/dev/null
    wait "${server_pid}"
    server_rc=$?
    set -e

    if [ "${writer_rc}" -ne 0 ]; then
        status="writer_failed"
        reason="stdio canary writer exited with code ${writer_rc}"
    elif [ "${server_rc}" -ne 0 ] && [ "${server_rc}" -ne 143 ]; then
        status="server_failed"
        reason="stdio canary server exited with code ${server_rc}"
    elif ! grep -q '"id":5' "${stdout_path}" 2>/dev/null; then
        status="missing_send_response"
        reason="stdio canary did not observe a send_message response"
    fi

    write_server_canary_status \
        "${status_path}" \
        "${status}" \
        "${reason}" \
        "${stdout_path}" \
        "${stderr_path}" \
        "${project_path}"
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
        --canary-db)
            CANARY_DB_PATH="${2:-}"
            CANARY_DB_EXPLICIT=1
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
server_canary_status_path="${OUTPUT_DIR}/server_canary_status.json"
server_canary_stdout_path="${OUTPUT_DIR}/server_canary_stdio.jsonl"
server_canary_stderr_path="${OUTPUT_DIR}/server_canary_stderr.log"

if [ "${SKIP_BENCH}" -eq 0 ]; then
    BENCH_REPORT_PATH="${OUTPUT_DIR}/bench_report.json"
    bench_runtime_root="$(mktemp -d "${BENCH_TMPDIR_ROOT}/atc-perf-gate.XXXXXX")"
    bench_workspace="${bench_runtime_root}/bench-workspace"
    bench_db_path="${bench_workspace}/bench.sqlite3"
    server_canary_db_path="${bench_runtime_root}/server-canary.sqlite3"
    if [ -z "${CANARY_DB_PATH}" ]; then
        CANARY_DB_PATH="${server_canary_db_path}"
    fi
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
            AM_BENCH_WORKSPACE="${bench_workspace}" \
            STORAGE_ROOT="${bench_runtime_root}/storage" \
            DATABASE_URL="sqlite:///${bench_db_path}" \
            "${bench_binary}" \
                bench \
                --json \
                --filter 'mail_send*' \
                --warmup 3 \
                --runs 50 \
                --baseline "${BASELINE_PATH}"
        else
            AM_BENCH_WORKSPACE="${bench_workspace}" \
            STORAGE_ROOT="${bench_runtime_root}/storage" \
            DATABASE_URL="sqlite:///${bench_db_path}" \
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
    if [ "${CANARY_DB_EXPLICIT}" -eq 1 ]; then
        write_server_canary_status \
            "${server_canary_status_path}" \
            "skipped" \
            "explicit --canary-db is inspected read-only and is not mutated by the server canary" \
            "${server_canary_stdout_path}" \
            "${server_canary_stderr_path}" \
            "/tmp/am-atc-perf-canary"
    elif [ "${bench_rc}" -eq 0 ] || [ "${bench_rc}" -eq 3 ]; then
        run_server_canary \
            "${CANARY_DB_PATH}" \
            "${bench_runtime_root}/storage" \
            "${OUTPUT_DIR}" \
            "${bench_binary}" \
            "${server_canary_status_path}"
    else
        write_server_canary_status \
            "${server_canary_status_path}" \
            "skipped" \
            "benchmark command failed before server canary" \
            "${server_canary_stdout_path}" \
            "${server_canary_stderr_path}" \
            "/tmp/am-atc-perf-canary"
    fi
else
    bench_rc=0
    cp "${BENCH_REPORT_PATH}" "${OUTPUT_DIR}/bench_report.json"
    BENCH_REPORT_PATH="${OUTPUT_DIR}/bench_report.json"
    printf 'skip-bench=1; reusing %s\n' "${BENCH_REPORT_PATH}" >"${bench_log}"
    write_server_canary_status \
        "${server_canary_status_path}" \
        "skipped" \
        "skip-bench reuses an existing report and does not mutate the supplied canary database" \
        "${server_canary_stdout_path}" \
        "${server_canary_stderr_path}" \
        "/tmp/am-atc-perf-canary"
fi

if [ -f "${HISTORICAL_BASELINE_PATH}" ]; then
    cp "${HISTORICAL_BASELINE_PATH}" "${OUTPUT_DIR}/atc_pre_wiring_baseline.json"
fi

python3 - "${BASELINE_PATH}" "${BENCH_REPORT_PATH}" "${OUTPUT_DIR}" "${bench_rc}" "${THRESHOLD_PCT}" "${CANARY_DB_PATH}" "${CANARY_REPORT_STATE_DIR}" "${server_canary_status_path}" "${server_canary_stdout_path}" "${server_canary_stderr_path}" <<'PY'
import json
import pathlib
import sqlite3
import sys

baseline_path = pathlib.Path(sys.argv[1])
bench_report_path = pathlib.Path(sys.argv[2])
output_dir = pathlib.Path(sys.argv[3])
bench_rc = int(sys.argv[4])
threshold_pct = float(sys.argv[5])
canary_db_arg = sys.argv[6]
canary_state_dir_arg = sys.argv[7]
canary_db_path = pathlib.Path(canary_db_arg) if canary_db_arg else None
canary_state_dir = pathlib.Path(canary_state_dir_arg) if canary_state_dir_arg else None
server_canary_status_path = pathlib.Path(sys.argv[8])
server_canary_stdout_path = pathlib.Path(sys.argv[9])
server_canary_stderr_path = pathlib.Path(sys.argv[10])

summary_path = output_dir / "summary.json"
comment_path = output_dir / "comment.md"
canary_report_path = output_dir / "canary_report.json"
latest_canary_report_path = (
    canary_state_dir / "latest_canary_report.json"
    if canary_state_dir is not None
    else None
)

required = [
    "mail_send_no_atc",
    "mail_send_atc_shadow",
    "mail_send_atc_live",
]

def read_db_health() -> dict:
    if canary_db_path is None:
        return {
            "checked": False,
            "path": None,
            "quick_check": "not_checked",
            "atc_rows": None,
            "reason": "no canary database path was provided",
        }
    if not canary_db_path.is_file():
        return {
            "checked": False,
            "path": str(canary_db_path),
            "quick_check": "not_found",
            "atc_rows": None,
            "reason": "canary database path does not exist",
        }
    try:
        conn = sqlite3.connect(f"file:{canary_db_path}?mode=ro", uri=True)
        try:
            quick_rows = conn.execute("PRAGMA quick_check").fetchall()
            quick_check = quick_rows[0][0] if quick_rows else "missing_result"
            has_atc = (
                conn.execute(
                    "SELECT 1 FROM sqlite_master WHERE type='table' AND name='atc_experiences'"
                ).fetchone()
                is not None
            )
            atc_rows = (
                int(conn.execute("SELECT COUNT(*) FROM atc_experiences").fetchone()[0])
                if has_atc
                else 0
            )
        finally:
            conn.close()
    except sqlite3.Error as error:
        return {
            "checked": True,
            "path": str(canary_db_path),
            "quick_check": "error",
            "atc_rows": None,
            "reason": str(error),
        }
    return {
        "checked": True,
        "path": str(canary_db_path),
        "quick_check": str(quick_check),
        "atc_rows": atc_rows,
        "reason": "",
    }


def read_server_canary_status() -> dict:
    if not server_canary_status_path.is_file():
        return {
            "status": "missing",
            "reason": "server canary status artifact was not written",
            "stdout": str(server_canary_stdout_path),
            "stderr": str(server_canary_stderr_path),
            "project": "/tmp/am-atc-perf-canary",
        }
    try:
        return json.loads(server_canary_status_path.read_text(encoding="utf-8"))
    except json.JSONDecodeError as error:
        return {
            "status": "invalid",
            "reason": str(error),
            "stdout": str(server_canary_stdout_path),
            "stderr": str(server_canary_stderr_path),
            "project": "/tmp/am-atc-perf-canary",
        }


def latency_entry(name: str, entry: dict, comparison: dict | None) -> dict:
    return {
        "name": name,
        "p50_ms": entry.get("median_ms"),
        "p95_ms": entry.get("p95_ms"),
        "p99_ms": entry.get("p99_ms"),
        "mean_ms": entry.get("mean_ms"),
        "delta_vs_no_atc_pct": None if comparison is None else comparison["delta_vs_no_atc_pct"],
        "overhead_regression": False if comparison is None else comparison["overhead_regression"],
    }


def fallback_decision(status: str, db_health: dict, overhead_regressions: list[str]) -> dict:
    if status in {"benchmark_failed", "runtime_error"}:
        return {
            "verdict": "hold_live",
            "applied": False,
            "reason": "benchmark or gate runtime failed",
            "recommendation": "Do not promote AM_ATC_WRITE_MODE=live; keep live rollout disabled until the gate succeeds.",
            "safe_command": "export AM_ATC_WRITE_MODE=shadow",
        }
    if db_health["quick_check"] not in {"ok", "not_checked"}:
        return {
            "verdict": "disable_live",
            "applied": False,
            "reason": f"canary database quick_check was {db_health['quick_check']}",
            "recommendation": "Disable live ATC writes and inspect the canary database before retrying.",
            "safe_command": "export AM_ATC_WRITE_MODE=shadow",
        }
    if "mail_send_atc_live" in overhead_regressions:
        return {
            "verdict": "disable_live",
            "applied": False,
            "reason": "live ATC p95 overhead exceeded the configured budget",
            "recommendation": "Disable or avoid live ATC writes; run shadow mode while investigating the regression.",
            "safe_command": "export AM_ATC_WRITE_MODE=shadow",
        }
    if overhead_regressions:
        return {
            "verdict": "hold_rollout",
            "applied": False,
            "reason": "an ATC mode exceeded the configured p95 overhead budget",
            "recommendation": "Hold ATC rollout and inspect the benchmark report before enabling live mode.",
            "safe_command": "export AM_ATC_WRITE_MODE=shadow",
        }
    if not db_health["checked"]:
        return {
            "verdict": "manual_review",
            "applied": False,
            "reason": db_health["reason"],
            "recommendation": "Benchmark SLOs passed, but database health was not checked; run with --canary-db or without --skip-bench before promoting live mode.",
            "safe_command": "scripts/bench_atc_perf_gate.sh",
        }
    if db_health["atc_rows"] == 0:
        return {
            "verdict": "hold_live",
            "applied": False,
            "reason": "canary database recorded no ATC experience rows",
            "recommendation": "Do not promote live ATC writes until the canary exercises the durable ATC experience path.",
            "safe_command": "export AM_ATC_WRITE_MODE=shadow",
        }
    return {
        "verdict": "canary_passed",
        "applied": False,
        "reason": "latency budget and database quick_check passed",
        "recommendation": "Live ATC writes are within this gate's canary budget; keep rollout controlled by explicit operator configuration.",
        "safe_command": "export AM_ATC_WRITE_MODE=live",
    }


def write_canary_report(
    status: str,
    reason: str,
    benchmarks: dict,
    comparisons: list[dict],
    baseline_regressions: list[str],
    failures: list[dict],
) -> dict:
    db_health = read_db_health()
    by_name = {item["name"]: item for item in comparisons}
    overhead_regressions = [item["name"] for item in comparisons if item["overhead_regression"]]
    latency = [
        latency_entry(name, benchmarks.get(name, {}), by_name.get(name))
        for name in required
    ]
    report = {
        "schema_version": 1,
        "status": status,
        "reason": reason,
        "artifacts": {
            "summary": str(summary_path),
            "comment": str(comment_path),
            "bench_report": str(bench_report_path),
            "baseline": str(baseline_path),
            "canary_db": db_health["path"],
            "canary_report": str(canary_report_path),
            "server_canary_status": str(server_canary_status_path),
            "server_canary_stdout": str(server_canary_stdout_path),
            "server_canary_stderr": str(server_canary_stderr_path),
            "latest_canary_report": (
                str(latest_canary_report_path)
                if latest_canary_report_path is not None
                else None
            ),
        },
        "slo": {
            "threshold_pct": threshold_pct,
            "required_benchmarks": required,
        },
        "latency": latency,
        "db_health": db_health,
        "baseline_regressions": baseline_regressions,
        "failures": failures,
        "server_canary": read_server_canary_status(),
        "fallback_decision": fallback_decision(status, db_health, overhead_regressions),
        "write_modes": {
            "mail_send_no_atc": {"ATC_LEARNING_DISABLED": "1"},
            "mail_send_atc_shadow": {"AM_ATC_WRITE_MODE": "shadow"},
            "mail_send_atc_live": {"AM_ATC_WRITE_MODE": "live"},
        },
    }
    canary_report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    if latest_canary_report_path is not None:
        latest_canary_report_path.parent.mkdir(parents=True, exist_ok=True)
        latest_canary_report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return report


def write_failure(status: str, reason: str) -> None:
    canary_report = write_canary_report(status, reason, {}, [], [], [])
    payload = {
        "schema_version": 1,
        "status": status,
        "reason": reason,
        "baseline_path": str(baseline_path),
        "bench_report_path": str(bench_report_path),
        "canary_report_path": str(canary_report_path),
        "bench_exit_code": bench_rc,
        "threshold_pct": threshold_pct,
        "comparisons": [],
        "baseline_regressions": [],
        "db_health": canary_report["db_health"],
        "fallback_decision": canary_report["fallback_decision"],
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
                f"- Canary report: `{canary_report_path}`",
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

canary_report = write_canary_report(
    status,
    reason,
    benchmarks,
    comparisons,
    baseline_regressions,
    failures,
)

payload = {
    "schema_version": 1,
    "status": status,
    "reason": reason,
    "baseline_path": str(baseline_path),
    "bench_report_path": str(bench_report_path),
    "canary_report_path": str(canary_report_path),
    "bench_exit_code": bench_rc,
    "threshold_pct": threshold_pct,
    "comparisons": comparisons,
    "baseline_regressions": baseline_regressions,
    "advisories": advisories,
    "failures": failures,
    "db_health": canary_report["db_health"],
    "fallback_decision": canary_report["fallback_decision"],
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
    f"- Canary report: `{canary_report_path.name}`",
    f"- Database quick_check: `{canary_report['db_health']['quick_check']}`",
    f"- ATC rows observed: `{canary_report['db_health']['atc_rows']}`",
    f"- Fallback verdict: `{canary_report['fallback_decision']['verdict']}`",
    "",
    "| Benchmark | p50 | p95 | p99 | Baseline p95 | Delta vs no_atc | Allowed vs no_atc | Verdict |",
    "|---|---:|---:|---:|---:|---:|---:|---|",
]

for item in comparisons:
    benchmark = benchmarks[item["name"]]
    verdict = "pass"
    if item["overhead_regression"]:
        verdict = "overhead regression"
    elif item["absolute_regression"]:
        verdict = "baseline drift (advisory)"
    lines.append(
        "| {name} | {p50} | {current_p95_ms:.2f}ms | {p99} | {baseline} | {delta:+.2f}% | {allowed:.2f}ms | {verdict} |".format(
            name=item["name"],
            p50=(
                f"{float(benchmark['median_ms']):.2f}ms"
                if benchmark.get("median_ms") is not None
                else "-"
            ),
            current_p95_ms=item["current_p95_ms"],
            p99=(
                f"{float(benchmark['p99_ms']):.2f}ms"
                if benchmark.get("p99_ms") is not None
                else "-"
            ),
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
