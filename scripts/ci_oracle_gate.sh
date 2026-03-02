#!/usr/bin/env bash
# ci_oracle_gate.sh - E5+H5: CI Oracle Hard Gate + Incident Non-Regression
#
# Strict CI gate: runs truth probe runner in deterministic mode,
# fails build on any non-whitelisted mismatch, publishes mismatch
# diff artifacts, culprit surface map, and regression class summary.
#
# E5 (br-2k3qx.5.5) incident-specific gates:
#   - false_empty:      DB has data but surface shows zero
#   - body_placeholder: message bodies are empty/placeholder
#   - auth_workflow:    system health auth regressions
# These regression classes can NEVER be whitelisted.
#
# Exit codes:
#   0  all probes pass, no mismatches
#   1  fatal error or non-whitelisted mismatches detected
#   2  only whitelisted mismatches detected (warning, not failure)
#
# Beads: br-2k3qx.5.5 (E5), br-2k3qx.8.5 (H5)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
PROBE_RUNNER="${SCRIPT_DIR}/truth_probe_runner.sh"
source "${SCRIPT_DIR}/truth_oracle_lib.sh"
TRUTH_ORACLE_LOG_PREFIX="ci-oracle-gate"

MODE="deterministic"
OUTPUT_DIR=""
WHITELIST_PATH=""
VERBOSE=0
ROBOT_TIMEOUT_SECS=10

usage() {
    cat <<'USAGE'
Usage: scripts/ci_oracle_gate.sh [options]

Options:
  --mode <deterministic|high-cardinality|both>  Probe mode (default: deterministic)
  --output-dir <path>                           Output directory (default: auto-timestamped)
  --whitelist <path>                            JSON whitelist of accepted mismatch check_ids
  --robot-timeout-secs <n>                      Timeout per robot command (default: 10)
  --verbose                                     Verbose logging
  -h, --help                                    Show help

Outputs:
  <output-dir>/
    gate_verdict.json           Machine-readable gate result
    culprit_surface_map.json    Surface-to-mismatch mapping
    mismatch_diffs.json         Expected vs observed diffs for failures
    probe/                      Full truth probe runner artifacts

Whitelist format (JSON array of check_id strings):
  ["robot.attachments:order_by_syntax", "db.integrity:idx_agent_links"]

Exit codes:
  0  Clean: no mismatches
  1  FAIL: non-whitelisted mismatches (build should fail)
  2  WARN: only whitelisted mismatches (build passes with warning)
USAGE
}

while [ $# -gt 0 ]; do
    case "$1" in
        --mode)
            MODE="${2:-}"
            shift 2
            ;;
        --output-dir)
            OUTPUT_DIR="${2:-}"
            shift 2
            ;;
        --whitelist)
            WHITELIST_PATH="${2:-}"
            shift 2
            ;;
        --robot-timeout-secs)
            ROBOT_TIMEOUT_SECS="${2:-}"
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

require_cmds python3 jq sqlite3 curl

[ -x "${PROBE_RUNNER}" ] || die "truth probe runner not found: ${PROBE_RUNNER}"

if [ -z "${OUTPUT_DIR}" ]; then
    OUTPUT_DIR="${PROJECT_ROOT}/tests/artifacts/ci_oracle_gate/$(date -u '+%Y%m%d_%H%M%S')"
fi
PROBE_DIR="${OUTPUT_DIR}/probe"
mkdir -p "${OUTPUT_DIR}" "${PROBE_DIR}"

# ── Step 1: Run truth probe runner ───────────────────────────────────────
log "running truth probe in ${MODE} mode"

PROBE_ARGS=(
    --mode "${MODE}"
    --output-dir "${PROBE_DIR}"
    --robot-timeout-secs "${ROBOT_TIMEOUT_SECS}"
)
if [ "${VERBOSE}" -eq 1 ]; then
    PROBE_ARGS+=(--verbose)
fi

set +e
"${PROBE_RUNNER}" "${PROBE_ARGS[@]}" \
    >"${OUTPUT_DIR}/probe_stdout.log" 2>"${OUTPUT_DIR}/probe_stderr.log"
PROBE_RC=$?
set -e

log "probe runner exited with code ${PROBE_RC}"

REPORT_PATH="${PROBE_DIR}/truth_probe_report.json"
if [ ! -f "${REPORT_PATH}" ]; then
    # Fatal: no report generated
    cat > "${OUTPUT_DIR}/gate_verdict.json" <<JSON
{
  "verdict": "ERROR",
  "reason": "truth probe runner failed to produce report (exit ${PROBE_RC})",
  "probe_exit_code": ${PROBE_RC},
  "bead_id": "br-2k3qx.5.5"
}
JSON
    printf 'CI ORACLE GATE: ERROR - no probe report generated (exit %d)\n' "${PROBE_RC}" >&2
    cat "${OUTPUT_DIR}/probe_stderr.log" >&2 || true
    exit 1
fi

# ── Step 2: Extract mismatches, classify by regression class, build artifacts ─
python3 - "${REPORT_PATH}" "${WHITELIST_PATH:-}" "${OUTPUT_DIR}" <<'PY'
import json
import sys
from collections import defaultdict
from pathlib import Path

report_path = sys.argv[1]
whitelist_path = sys.argv[2] if len(sys.argv) > 2 and sys.argv[2] else ""
output_dir = Path(sys.argv[3])

with open(report_path, "r", encoding="utf-8") as f:
    report = json.load(f)

# Load whitelist
whitelist: set[str] = set()
if whitelist_path:
    try:
        with open(whitelist_path, "r", encoding="utf-8") as wf:
            wl = json.load(wf)
            if isinstance(wl, list):
                whitelist = set(wl)
    except (FileNotFoundError, json.JSONDecodeError):
        pass

# E5 (br-2k3qx.5.5): Regression classes that can NEVER be whitelisted.
# These represent P0 correctness failures that must always fail the gate.
NEVER_WHITELIST_CLASSES = {"false_empty", "body_placeholder", "auth_workflow"}

# Collect all checks (probe report uses "status" field, not "verdict")
checks = report.get("checks", [])
mismatches = [c for c in checks if c.get("status") in ("mismatch", "fail", "FAIL") or c.get("verdict") in ("mismatch", "fail", "FAIL")]
indeterminate = [c for c in checks if c.get("status") in ("indeterminate", "UNKNOWN") or c.get("verdict") in ("indeterminate", "UNKNOWN")]
passing = [c for c in checks if c.get("status") in ("match", "pass", "PASS", "ok") or c.get("verdict") in ("pass", "PASS", "ok")]

# Classify mismatches: whitelist + regression class enforcement
whitelisted_mismatches = []
non_whitelisted_mismatches = []
never_whitelisted_mismatches = []
for m in mismatches:
    check_id = m.get("check_id", "unknown")
    regression_class = m.get("regression_class", "")
    # Checks with NEVER_WHITELIST_CLASSES can never be whitelisted
    if regression_class in NEVER_WHITELIST_CLASSES:
        never_whitelisted_mismatches.append(m)
        non_whitelisted_mismatches.append(m)
    elif check_id in whitelist:
        whitelisted_mismatches.append(m)
    else:
        non_whitelisted_mismatches.append(m)

# Build culprit surface map
surface_map: dict[str, list[dict]] = {}
for m in mismatches + indeterminate:
    surface = m.get("surface_id", m.get("surface", m.get("check_id", "unknown").split(":")[0]))
    surface_map.setdefault(surface, []).append({
        "check_id": m.get("check_id"),
        "verdict": m.get("status", m.get("verdict")),
        "expected": m.get("db_truth", m.get("expected")),
        "observed": m.get("surface_value", m.get("observed")),
        "whitelisted": m.get("check_id", "") in whitelist and m.get("regression_class", "") not in NEVER_WHITELIST_CLASSES,
        "regression_class": m.get("regression_class"),
    })

with open(output_dir / "culprit_surface_map.json", "w") as f:
    json.dump(surface_map, f, indent=2, sort_keys=True)

# Build mismatch diffs
diffs = []
for m in mismatches:
    diffs.append({
        "check_id": m.get("check_id"),
        "surface": m.get("surface_id", m.get("surface", "unknown")),
        "expected": m.get("db_truth", m.get("expected")),
        "observed": m.get("surface_value", m.get("observed")),
        "detail": m.get("note", m.get("detail", m.get("message", ""))),
        "whitelisted": m.get("check_id", "") in whitelist and m.get("regression_class", "") not in NEVER_WHITELIST_CLASSES,
        "regression_class": m.get("regression_class"),
    })

with open(output_dir / "mismatch_diffs.json", "w") as f:
    json.dump(diffs, f, indent=2, sort_keys=True)

# E5: Build per-regression-class summary
regression_class_summary: dict[str, dict] = defaultdict(lambda: {"total": 0, "mismatches": 0, "check_ids": []})
for c in checks:
    rc = c.get("regression_class")
    if rc:
        regression_class_summary[rc]["total"] += 1
        if c.get("status") in ("mismatch", "fail", "FAIL"):
            regression_class_summary[rc]["mismatches"] += 1
            regression_class_summary[rc]["check_ids"].append(c.get("check_id"))

with open(output_dir / "regression_class_summary.json", "w") as f:
    json.dump(dict(regression_class_summary), f, indent=2, sort_keys=True)

# Determine verdict
if non_whitelisted_mismatches:
    verdict = "FAIL"
elif whitelisted_mismatches or indeterminate:
    verdict = "WARN"
else:
    verdict = "PASS"

gate_result = {
    "verdict": verdict,
    "bead_id": "br-2k3qx.5.5",
    "total_checks": len(checks),
    "passing": len(passing),
    "mismatches": len(mismatches),
    "whitelisted_mismatches": len(whitelisted_mismatches),
    "non_whitelisted_mismatches": len(non_whitelisted_mismatches),
    "never_whitelisted_mismatches": len(never_whitelisted_mismatches),
    "indeterminate": len(indeterminate),
    "culprit_surfaces": sorted(surface_map.keys()),
    "whitelist_path": whitelist_path or "(none)",
    "regression_classes": dict(regression_class_summary),
}

with open(output_dir / "gate_verdict.json", "w") as f:
    json.dump(gate_result, f, indent=2, sort_keys=True)

# Human-readable CI summary
print(f"\n{'=' * 60}", file=sys.stderr)
print(f"  CI ORACLE GATE: {verdict}", file=sys.stderr)
print(f"{'=' * 60}", file=sys.stderr)
print(f"  Total checks:              {len(checks)}", file=sys.stderr)
print(f"  Passing:                   {len(passing)}", file=sys.stderr)
print(f"  Mismatches:                {len(mismatches)}", file=sys.stderr)
print(f"    Whitelisted:             {len(whitelisted_mismatches)}", file=sys.stderr)
print(f"    Non-whitelisted (FAIL):  {len(non_whitelisted_mismatches)}", file=sys.stderr)
print(f"    Never-whitelist (P0):    {len(never_whitelisted_mismatches)}", file=sys.stderr)
print(f"  Indeterminate:             {len(indeterminate)}", file=sys.stderr)
if surface_map:
    print(f"  Culprit surfaces:          {', '.join(sorted(surface_map.keys()))}", file=sys.stderr)
print(f"  Artifacts:                 {output_dir}", file=sys.stderr)
print(f"{'=' * 60}", file=sys.stderr)

# E5: Per-regression-class breakdown
if regression_class_summary:
    print("\n  Regression class breakdown:", file=sys.stderr)
    for cls_name in sorted(regression_class_summary.keys()):
        cls = regression_class_summary[cls_name]
        status_label = "FAIL" if cls["mismatches"] > 0 else "PASS"
        never_wl = " [NEVER-WHITELIST]" if cls_name in NEVER_WHITELIST_CLASSES else ""
        print(f"    {cls_name}: {cls['mismatches']}/{cls['total']} mismatches ({status_label}){never_wl}", file=sys.stderr)
        for cid in cls["check_ids"][:5]:
            print(f"      - {cid}", file=sys.stderr)
        if len(cls["check_ids"]) > 5:
            print(f"      ... and {len(cls['check_ids']) - 5} more", file=sys.stderr)

if non_whitelisted_mismatches:
    print("\n  Non-whitelisted mismatches:", file=sys.stderr)
    for m in non_whitelisted_mismatches[:10]:
        cid = m.get("check_id", "?")
        exp = m.get("db_truth", m.get("expected", "?"))
        obs = m.get("surface_value", m.get("observed", "?"))
        rc = m.get("regression_class", "")
        rc_tag = f" [{rc}]" if rc else ""
        print(f"    - {cid}: expected={exp} observed={obs}{rc_tag}", file=sys.stderr)
    if len(non_whitelisted_mismatches) > 10:
        print(f"    ... and {len(non_whitelisted_mismatches) - 10} more", file=sys.stderr)

PY
GATE_EXIT=$?

if [ "${GATE_EXIT}" -ne 0 ]; then
    printf 'CI ORACLE GATE: python analysis failed\n' >&2
    exit 1
fi

FINAL_VERDICT="$(jq -r '.verdict' "${OUTPUT_DIR}/gate_verdict.json" 2>/dev/null)"

case "${FINAL_VERDICT}" in
    PASS)
        log "gate verdict: PASS"
        exit 0
        ;;
    WARN)
        log "gate verdict: WARN (whitelisted mismatches only)"
        exit 2
        ;;
    FAIL)
        log "gate verdict: FAIL (non-whitelisted mismatches detected)"
        exit 1
        ;;
    *)
        log "gate verdict: ERROR (unexpected: ${FINAL_VERDICT})"
        exit 1
        ;;
esac
