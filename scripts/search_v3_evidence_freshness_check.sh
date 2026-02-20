#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

ARTIFACTS_ROOT="${PROJECT_ROOT}/tests/artifacts"
OUTPUT_PATH="${ARTIFACTS_ROOT}/search_v3_freshness/latest.json"
STRICT=0

DAILY_MAX_AGE_S="${SEARCH_V3_FRESHNESS_DAILY_MAX_AGE_S:-86400}"
WEEKLY_MAX_AGE_S="${SEARCH_V3_FRESHNESS_WEEKLY_MAX_AGE_S:-604800}"
CHECKPOINT_MAX_AGE_S="${SEARCH_V3_FRESHNESS_CHECKPOINT_MAX_AGE_S:-2592000}"

usage() {
    cat <<'EOF'
Usage: scripts/search_v3_evidence_freshness_check.sh [options]

Detect stale or missing Search V3 operational evidence across tests/artifacts/search_v3_* roots.

Options:
  --artifacts-root <path>           Artifact root to scan (default: tests/artifacts)
  --output <path>                   JSON report output path
  --daily-max-age-s <seconds>       Daily snapshot freshness threshold (default: 86400)
  --weekly-max-age-s <seconds>      Weekly bundle freshness threshold (default: 604800)
  --checkpoint-max-age-s <seconds>  Post-cutover checkpoint freshness threshold (default: 2592000)
  --strict                          Exit non-zero when stale/missing evidence is found
  --help                            Show help

Environment:
  SEARCH_V3_FRESHNESS_DAILY_MAX_AGE_S
  SEARCH_V3_FRESHNESS_WEEKLY_MAX_AGE_S
  SEARCH_V3_FRESHNESS_CHECKPOINT_MAX_AGE_S
EOF
}

while [ $# -gt 0 ]; do
    case "$1" in
        --artifacts-root)
            ARTIFACTS_ROOT="$2"
            shift 2
            ;;
        --output)
            OUTPUT_PATH="$2"
            shift 2
            ;;
        --daily-max-age-s)
            DAILY_MAX_AGE_S="$2"
            shift 2
            ;;
        --weekly-max-age-s)
            WEEKLY_MAX_AGE_S="$2"
            shift 2
            ;;
        --checkpoint-max-age-s)
            CHECKPOINT_MAX_AGE_S="$2"
            shift 2
            ;;
        --strict)
            STRICT=1
            shift
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

mkdir -p "$(dirname "${OUTPUT_PATH}")"

TMP_REPORT="$(mktemp "${TMPDIR:-/tmp}/search_v3_freshness.XXXXXX.json")"
trap 'rm -f "${TMP_REPORT}"' EXIT

NOW_EPOCH="$(date +%s)"

python3 - "${ARTIFACTS_ROOT}" "${NOW_EPOCH}" "${DAILY_MAX_AGE_S}" "${WEEKLY_MAX_AGE_S}" "${CHECKPOINT_MAX_AGE_S}" "${TMP_REPORT}" <<'PY'
import json
import pathlib
import sys
from datetime import datetime, timezone

artifacts_root = pathlib.Path(sys.argv[1]).resolve()
now_epoch = int(sys.argv[2])
daily_max_age_s = int(sys.argv[3])
weekly_max_age_s = int(sys.argv[4])
checkpoint_max_age_s = int(sys.argv[5])
report_path = pathlib.Path(sys.argv[6])

required_files = [
    "search_v3/summaries/suite_summary.json",
    "metrics.json",
    "bundle.json",
    "repro.txt",
    "repro.env",
]

suite_checks = [
    {
        "category": "daily_snapshot",
        "suite": "search_v3_stdio",
        "threshold_seconds": daily_max_age_s,
        "remediation_command": "bash tests/e2e/test_search_v3_stdio.sh",
    },
    {
        "category": "daily_snapshot",
        "suite": "search_v3_http",
        "threshold_seconds": daily_max_age_s,
        "remediation_command": "bash tests/e2e/test_search_v3_http.sh",
    },
    {
        "category": "daily_snapshot",
        "suite": "search_v3_resilience",
        "threshold_seconds": daily_max_age_s,
        "remediation_command": "bash tests/e2e/test_search_v3_resilience.sh",
    },
    {
        "category": "weekly_suite_bundle",
        "suite": "search_v3_load_concurrency",
        "threshold_seconds": weekly_max_age_s,
        "remediation_command": "bash tests/e2e/test_search_v3_load_concurrency.sh",
    },
    {
        "category": "post_cutover_checkpoint",
        "suite": "search_v3_shadow_parity",
        "threshold_seconds": checkpoint_max_age_s,
        "remediation_command": "bash tests/e2e/test_search_v3_shadow_parity.sh",
    },
]

checks = []
failed_checks = 0
stale_file_count = 0
missing_file_count = 0

for spec in suite_checks:
    suite_root = artifacts_root / spec["suite"]
    runs = []
    if suite_root.exists():
        runs = sorted([p for p in suite_root.iterdir() if p.is_dir()])
    latest_run = runs[-1] if runs else None

    structured_root = artifacts_root / "search_v3" / spec["suite"]
    structured_runs = []
    if structured_root.exists():
        structured_runs = sorted([p for p in structured_root.iterdir() if p.is_dir()])
    latest_structured_run = structured_runs[-1] if structured_runs else None

    check = {
        "category": spec["category"],
        "suite": spec["suite"],
        "suite_root": str(suite_root),
        "latest_run": str(latest_run) if latest_run else None,
        "structured_root": str(structured_root),
        "latest_structured_run": str(latest_structured_run) if latest_structured_run else None,
        "threshold_seconds": spec["threshold_seconds"],
        "remediation_command": spec["remediation_command"],
        "required_files": [],
        "status": "ok",
    }

    stale_or_missing = []

    if latest_run is None:
        check["status"] = "missing"
        stale_or_missing.append(
            {
                "path": str(suite_root),
                "relative_path": ".",
                "status": "missing",
                "age_seconds": None,
                "threshold_seconds": spec["threshold_seconds"],
                "remediation_command": spec["remediation_command"],
                "reason": "no artifact runs found for suite root",
            }
        )
        missing_file_count += 1
    else:
        for rel in required_files:
            candidates = [latest_run / rel]
            if rel == "search_v3/summaries/suite_summary.json" and latest_structured_run is not None:
                candidates.append(latest_structured_run / "summaries" / "suite_summary.json")
                candidates.append(latest_structured_run / "summary.json")

            existing_candidates = [c for c in candidates if c.exists()]
            path = existing_candidates[0] if existing_candidates else candidates[0]
            entry = {
                "relative_path": rel,
                "path": str(path),
                "status": "ok",
                "age_seconds": None,
                "threshold_seconds": spec["threshold_seconds"],
            }
            if not path.exists():
                entry["status"] = "missing"
                stale_or_missing.append(
                    {
                        **entry,
                        "remediation_command": spec["remediation_command"],
                        "reason": "required file missing in latest artifact bundle",
                    }
                )
                missing_file_count += 1
            else:
                mtime = int(path.stat().st_mtime)
                age = max(0, now_epoch - mtime)
                entry["age_seconds"] = age
                if age > spec["threshold_seconds"]:
                    entry["status"] = "stale"
                    stale_or_missing.append(
                        {
                            **entry,
                            "remediation_command": spec["remediation_command"],
                            "reason": "required file exceeds freshness threshold",
                        }
                    )
                    stale_file_count += 1
            check["required_files"].append(entry)

        if stale_or_missing:
            check["status"] = "missing" if any(i["status"] == "missing" for i in stale_or_missing) else "stale"

    check["stale_or_missing"] = stale_or_missing
    if check["status"] != "ok":
        failed_checks += 1
    checks.append(check)

report = {
    "schema_version": 1,
    "generated_at": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    "artifacts_root": str(artifacts_root),
    "thresholds_seconds": {
        "daily_snapshot": daily_max_age_s,
        "weekly_suite_bundle": weekly_max_age_s,
        "post_cutover_checkpoint": checkpoint_max_age_s,
    },
    "checks": checks,
    "summary": {
        "total_checks": len(checks),
        "failed_checks": failed_checks,
        "stale_files": stale_file_count,
        "missing_files": missing_file_count,
        "status": "pass" if failed_checks == 0 else "alert",
    },
}

report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
PY

cp "${TMP_REPORT}" "${OUTPUT_PATH}"

python3 - "${TMP_REPORT}" <<'PY'
import json
import sys
from pathlib import Path

report = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
print(json.dumps(report, indent=2))
PY

FAILED_CHECKS="$(python3 - "${TMP_REPORT}" <<'PY'
import json
import sys
from pathlib import Path

report = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
print(report.get("summary", {}).get("failed_checks", 0))
PY
)"

if [ "${FAILED_CHECKS}" -gt 0 ]; then
    echo "Search V3 evidence freshness ALERT: ${FAILED_CHECKS} check(s) require attention." >&2
    python3 - "${TMP_REPORT}" <<'PY' >&2
import json
import sys
from pathlib import Path

report = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
for check in report.get("checks", []):
    if check.get("status") == "ok":
        continue
    print(f"- [{check.get('category')}] {check.get('suite')} status={check.get('status')}")
    for finding in check.get("stale_or_missing", []):
        age = finding.get("age_seconds")
        age_text = "n/a" if age is None else f"{age}s"
        print(
            f"  path={finding.get('path')} age={age_text} threshold={finding.get('threshold_seconds')}s"
        )
        print(f"  remediation: {finding.get('remediation_command')}")
PY
    if [ "${STRICT}" -eq 1 ]; then
        exit 1
    fi
fi

exit 0
