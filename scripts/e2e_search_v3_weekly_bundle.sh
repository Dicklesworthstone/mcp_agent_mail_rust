#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
cd "${PROJECT_ROOT}"

RUN_TS="$(date -u '+%Y%m%d_%H%M%S')"
SEED="${E2E_SEED:-424242}"
CLOCK_MODE="${E2E_CLOCK_MODE:-deterministic}"
STRICT_FRESHNESS=0

usage() {
    cat <<'EOF'
Usage: scripts/e2e_search_v3_weekly_bundle.sh [options]

Run the weekly Search V3 steady-state bundle and emit machine-readable rollup output.

Options:
  --strict-freshness     Fail when freshness checker reports alerts
  --seed <n>             Deterministic seed (default: 424242)
  --clock-mode <mode>    E2E clock mode (default: deterministic)
  --run-ts <timestamp>   Override run timestamp (UTC yyyymmdd_HHMMSS)
  --help                 Show help
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --strict-freshness)
            STRICT_FRESHNESS=1
            shift
            ;;
        --seed)
            SEED="$2"
            shift 2
            ;;
        --clock-mode)
            CLOCK_MODE="$2"
            shift 2
            ;;
        --run-ts)
            RUN_TS="$2"
            shift 2
            ;;
        --help|-h)
            usage
            exit 0
            ;;
        *)
            echo "Unknown option: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

WEEKLY_ROOT="tests/artifacts/search_v3_weekly/${RUN_TS}"
LOG_ROOT="${WEEKLY_ROOT}/logs"
STATUS_ROOT="${WEEKLY_ROOT}/status"
ROLLUP_JSON="${WEEKLY_ROOT}/rollup.json"
ROLLUP_TXT="${WEEKLY_ROOT}/rollup.txt"
FRESHNESS_REPORT="tests/artifacts/search_v3_freshness/latest.json"
FRESHNESS_LOG="${LOG_ROOT}/search_v3_freshness.log"

mkdir -p "${LOG_ROOT}" "${STATUS_ROOT}"

SUITES=(
    "tests/e2e/test_search_v3_stdio.sh"
    "tests/e2e/test_search_v3_http.sh"
    "tests/e2e/test_search_v3_resilience.sh"
    "tests/e2e/test_search_v3_load_concurrency.sh"
)

FAILED=0

run_suite() {
    local suite_script="$1"
    local suite_name
    suite_name="$(basename "${suite_script}" .sh)"
    local log_file="${LOG_ROOT}/${suite_name}.log"
    local status_file="${STATUS_ROOT}/${suite_name}.status"

    echo "==> Running ${suite_name}"
    if E2E_CLOCK_MODE="${CLOCK_MODE}" \
        E2E_SEED="${SEED}" \
        SEARCH_V3_LOG_ROOT="${PWD}/tests/artifacts/search_v3" \
        SV3_ARTIFACT_ROOT="${PWD}/tests/artifacts/search_v3" \
        bash "${suite_script}" >"${log_file}" 2>&1; then
        echo "pass" > "${status_file}"
    else
        echo "fail" > "${status_file}"
        FAILED=1
    fi
}

for suite in "${SUITES[@]}"; do
    run_suite "${suite}"
done

freshness_args=(--output "${FRESHNESS_REPORT}")
if [ "${STRICT_FRESHNESS}" -eq 1 ]; then
    freshness_args+=(--strict)
fi

if scripts/search_v3_evidence_freshness_check.sh \
    "${freshness_args[@]}" \
    >"${FRESHNESS_LOG}" 2>&1; then
    echo "pass" > "${STATUS_ROOT}/freshness.status"
else
    echo "fail" > "${STATUS_ROOT}/freshness.status"
    if [ "${STRICT_FRESHNESS}" -eq 1 ]; then
        FAILED=1
    fi
fi

python3 - "${WEEKLY_ROOT}" "${ROLLUP_JSON}" "${ROLLUP_TXT}" "${FRESHNESS_REPORT}" "${STRICT_FRESHNESS}" <<'PY'
import glob
import json
import sys
from datetime import datetime, timezone
from pathlib import Path

weekly_root = Path(sys.argv[1])
rollup_json = Path(sys.argv[2])
rollup_txt = Path(sys.argv[3])
freshness_report_path = Path(sys.argv[4])
strict_freshness = sys.argv[5] == "1"

artifact_root = Path("tests/artifacts")
status_root = weekly_root / "status"

suite_specs = [
    ("search_v3_stdio", "tests/e2e/test_search_v3_stdio.sh", 80),
    ("search_v3_http", "tests/e2e/test_search_v3_http.sh", 90),
    ("search_v3_resilience", "tests/e2e/test_search_v3_resilience.sh", 70),
    ("search_v3_load_concurrency", "tests/e2e/test_search_v3_load_concurrency.sh", 60),
]

def newest(pattern: str):
    matches = sorted(glob.glob(pattern))
    if not matches:
        return None
    return Path(matches[-1])

suites = []
overall_failures = []

for suite_name, script_path, assertion_floor in suite_specs:
    status_file = status_root / f"{suite_name}.status"
    run_status = status_file.read_text(encoding="utf-8").strip() if status_file.exists() else "missing"

    summary_path = newest(f"tests/artifacts/{suite_name}/*/summary.json")
    summary = {}
    if summary_path and summary_path.exists():
        summary = json.loads(summary_path.read_text(encoding="utf-8"))

    total = int(summary.get("total", -1)) if summary else -1
    passed = int(summary.get("pass", -1)) if summary else -1
    failed = int(summary.get("fail", -1)) if summary else -1

    floor_ok = total >= assertion_floor and failed == 0

    contract_files = []
    if summary_path:
        suite_dir = summary_path.parent
        for rel in ("metrics.json", "bundle.json", "repro.txt", "repro.env"):
            p = suite_dir / rel
            contract_files.append({"path": str(p), "exists": p.exists()})
    structured_suite_summary = newest(
        f"tests/artifacts/search_v3/{suite_name}/*/summaries/suite_summary.json"
    )
    if structured_suite_summary is None:
        structured_suite_summary = newest(
            f"tests/artifacts/search_v3/{suite_name}/*/summary.json"
        )
    contract_files.append(
        {
            "path": str(structured_suite_summary) if structured_suite_summary else "",
            "exists": structured_suite_summary is not None and structured_suite_summary.exists(),
        }
    )

    missing_contract = [f["path"] for f in contract_files if not f["exists"]]
    suite_ok = run_status == "pass" and floor_ok and not missing_contract

    if not suite_ok:
        overall_failures.append(suite_name)

    suites.append(
        {
            "suite": suite_name,
            "script": script_path,
            "run_status": run_status,
            "assertion_floor": assertion_floor,
            "assertions_total": total,
            "assertions_passed": passed,
            "assertions_failed": failed,
            "floor_ok": floor_ok,
            "summary_path": str(summary_path) if summary_path else None,
            "contract_ok": len(missing_contract) == 0,
            "missing_contract_files": missing_contract,
            "status": "pass" if suite_ok else "fail",
        }
    )

freshness = {}
freshness_status = "missing"
if freshness_report_path.exists():
    freshness = json.loads(freshness_report_path.read_text(encoding="utf-8"))
    freshness_status = freshness.get("summary", {}).get("status", "missing")

load_guardrails = {
    "write_p95_ms": None,
    "search_p95_ms": None,
    "freshness_p95_ms": None,
}
load_summary_path = newest(
    "tests/artifacts/search_v3/search_v3_load_concurrency/*/load/load_summary.json"
)
if load_summary_path and load_summary_path.exists():
    load_data = json.loads(load_summary_path.read_text(encoding="utf-8"))
    load_guardrails["write_p95_ms"] = load_data.get("writes", {}).get("p95_ms")
    load_guardrails["search_p95_ms"] = load_data.get("searches", {}).get("p95_ms")

freshness_stats_path = newest(
    "tests/artifacts/search_v3/search_v3_load_concurrency/*/load/freshness_lag_stats.json"
)
if freshness_stats_path and freshness_stats_path.exists():
    freshness_stats = json.loads(freshness_stats_path.read_text(encoding="utf-8"))
    load_guardrails["freshness_p95_ms"] = freshness_stats.get("p95_ms")

relevance_guardrails = {
    "stdio_floor_ok": next((s["floor_ok"] for s in suites if s["suite"] == "search_v3_stdio"), False),
    "http_floor_ok": next((s["floor_ok"] for s in suites if s["suite"] == "search_v3_http"), False),
}

if strict_freshness and freshness_status != "pass":
    overall_failures.append("search_v3_freshness")

rollup = {
    "schema_version": 1,
    "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "run_root": str(weekly_root),
    "suites": suites,
    "guardrails": {
        "latency": load_guardrails,
        "relevance": relevance_guardrails,
    },
    "freshness": {
        "strict": strict_freshness,
        "status": freshness_status,
        "report_path": str(freshness_report_path),
    },
    "summary": {
        "suite_count": len(suites),
        "failed_suites": [s["suite"] for s in suites if s["status"] != "pass"],
        "overall_status": "pass" if not overall_failures else "fail",
    },
}

rollup_json.write_text(json.dumps(rollup, indent=2) + "\n", encoding="utf-8")

lines = [
    "Search V3 Weekly Bundle Rollup",
    "==============================",
    f"Generated: {rollup['generated_at']}",
    f"Overall: {rollup['summary']['overall_status']}",
    "",
]
for suite in suites:
    lines.append(
        f"- {suite['suite']}: status={suite['status']} floor_ok={suite['floor_ok']} "
        f"assertions(total/pass/fail)={suite['assertions_total']}/{suite['assertions_passed']}/{suite['assertions_failed']}"
    )
if rollup["summary"]["failed_suites"]:
    lines.append("")
    lines.append("Failed suites:")
    for failed_suite in rollup["summary"]["failed_suites"]:
        lines.append(f"- {failed_suite}")
lines.append("")
lines.append(f"Freshness status: {freshness_status} (strict={strict_freshness})")
lines.append(f"Freshness report: {freshness_report_path}")
lines.append(
    "Latency guardrails (load_concurrency): "
    f"write_p95_ms={load_guardrails['write_p95_ms']} "
    f"search_p95_ms={load_guardrails['search_p95_ms']} "
    f"freshness_p95_ms={load_guardrails['freshness_p95_ms']}"
)
rollup_txt.write_text("\n".join(lines) + "\n", encoding="utf-8")
PY

echo "Weekly rollup written: ${ROLLUP_JSON}"
echo "Weekly summary written: ${ROLLUP_TXT}"
echo "Freshness report: ${FRESHNESS_REPORT}"

if [ "${FAILED}" -ne 0 ]; then
    exit 1
fi

OVERALL_STATUS="$(python3 - "${ROLLUP_JSON}" <<'PY'
import json
import sys
from pathlib import Path

rollup = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
print(rollup.get("summary", {}).get("overall_status", "fail"))
PY
)"

if [ "${OVERALL_STATUS}" != "pass" ]; then
    exit 1
fi

exit 0
