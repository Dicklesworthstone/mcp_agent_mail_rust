#!/usr/bin/env bash
# test_migration_ux_smoke.sh - UX consistency + onboarding smoke suite for native command migration
#
# Bead: br-1132.1
#
# Goal:
#   - Check cross-command UX consistency for migrated native commands
#     (help text, flag semantics, exit-code messaging, remediation cues)
#   - Run onboarding smoke scenarios for common first-run and failure flows
#   - Emit detailed machine-readable artifacts per scenario

set -euo pipefail

E2E_SUITE="migration_ux_smoke"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Migration UX Consistency + Onboarding Smoke (br-1132.1)"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

WORK="$(e2e_mktemp "e2e_migration_ux")"
FINDINGS_FILE="${WORK}/findings.jsonl"
touch "${FINDINGS_FILE}"

now_ms() {
    python3 - <<'PY'
import time
print(int(time.time() * 1000))
PY
}

run_case() {
    local case_id="$1"
    shift
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local -a cmd=( "$@" )
    local command_pretty
    local start_ms
    local end_ms
    local elapsed_ms
    local rc

    mkdir -p "${case_dir}"

    printf '%q ' "${cmd[@]}" > "${case_dir}/command.txt"
    command_pretty="$(cat "${case_dir}/command.txt")"

    python3 - <<'PY' \
        "${case_id}" "${E2E_SUITE}" "${E2E_RUN_STARTED_AT}" "${E2E_CLOCK_MODE}" \
        "${E2E_SEED}" "${E2E_RUN_START_EPOCH_S}" > "${case_dir}/environment.json"
import json
import os
import sys

payload = {
    "case_id": sys.argv[1],
    "suite": sys.argv[2],
    "run_started_at": sys.argv[3],
    "clock_mode": sys.argv[4],
    "seed": int(sys.argv[5]),
    "run_start_epoch_s": int(sys.argv[6]),
    "cwd": os.getcwd(),
    "env": {
        "CARGO_TARGET_DIR": os.getenv("CARGO_TARGET_DIR", ""),
        "RUST_LOG": os.getenv("RUST_LOG", ""),
        "AM_INTERFACE_MODE": os.getenv("AM_INTERFACE_MODE", ""),
    },
}
print(json.dumps(payload, indent=2))
PY

    start_ms="$(now_ms)"
    set +e
    "${cmd[@]}" > "${case_dir}/stdout.txt" 2> "${case_dir}/stderr.txt"
    rc=$?
    set -e
    end_ms="$(now_ms)"
    elapsed_ms=$((end_ms - start_ms))

    echo "${rc}" > "${case_dir}/exit_code.txt"
    echo "${elapsed_ms}" > "${case_dir}/timing_ms.txt"

    python3 - <<'PY' \
        "${case_id}" "${rc}" "${elapsed_ms}" "${command_pretty}" "${cmd[@]}" \
        > "${case_dir}/result.json"
import json
import sys

case_id = sys.argv[1]
exit_code = int(sys.argv[2])
elapsed_ms = int(sys.argv[3])
command_shell = sys.argv[4].strip()
argv = sys.argv[5:]

payload = {
    "case_id": case_id,
    "exit_code": exit_code,
    "elapsed_ms": elapsed_ms,
    "command": {
        "argv": argv,
        "shell": command_shell,
    },
    "artifacts": {
        "stdout": "stdout.txt",
        "stderr": "stderr.txt",
        "timing_ms": "timing_ms.txt",
        "exit_code": "exit_code.txt",
        "environment": "environment.json",
    },
}
print(json.dumps(payload, indent=2))
PY

    CASE_ID="${case_id}"
    CASE_DIR="${case_dir}"
    CASE_RC="${rc}"
    CASE_STDOUT="$(cat "${case_dir}/stdout.txt" 2>/dev/null || true)"
    CASE_STDERR="$(cat "${case_dir}/stderr.txt" 2>/dev/null || true)"

    e2e_log "case=${CASE_ID} rc=${CASE_RC} elapsed_ms=${elapsed_ms}"
}

add_finding() {
    local severity="$1"
    local category="$2"
    local case_id="$3"
    local summary="$4"
    local recommendation="$5"
    python3 - <<'PY' \
        "${severity}" "${category}" "${case_id}" "${summary}" "${recommendation}" \
        >> "${FINDINGS_FILE}"
import json
import sys

print(json.dumps({
    "severity": sys.argv[1],
    "category": sys.argv[2],
    "case_id": sys.argv[3],
    "summary": sys.argv[4],
    "recommendation": sys.argv[5],
}))
PY
}

assert_valid_json() {
    local label="$1"
    local file="$2"
    if python3 -m json.tool < "${file}" >/dev/null 2>&1; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
    fi
}

# ===========================================================================
# Case 1: Help-text consistency matrix
# ===========================================================================
e2e_case_banner "Help-text consistency across migrated commands"

while IFS='|' read -r case_id expects_json cmdline; do
    [ -n "${case_id}" ] || continue
    # shellcheck disable=SC2206
    cmd=( ${cmdline} )
    run_case "${case_id}" "${cmd[@]}"

    e2e_assert_exit_code "${case_id}: --help exits 0" "0" "${CASE_RC}"
    e2e_assert_contains "${case_id}: usage block present" "${CASE_STDOUT}" "Usage:"
    e2e_assert_contains "${case_id}: --help flag documented" "${CASE_STDOUT}" "--help"

    if [ "${expects_json}" = "yes" ]; then
        if [[ "${CASE_STDOUT}" == *"--json"* ]]; then
            e2e_pass "${case_id}: machine-readable flag surfaced"
        else
            e2e_fail "${case_id}: expected --json flag for machine output"
            add_finding "error" "help_consistency" "${case_id}" \
                "Expected --json support in help output but it was missing." \
                "Align help + flag semantics with other migrated commands that emit machine-readable output."
        fi
    else
        if [[ "${CASE_STDOUT}" == *"--json"* ]]; then
            add_finding "warning" "help_consistency" "${case_id}" \
                "Command appears interactive but exposes --json unexpectedly." \
                "Confirm whether interactive command should expose machine-readable output."
        fi
    fi
done <<'EOF'
help_ci|yes|am ci --help
help_flake_scan|yes|am flake-triage scan --help
help_flake_detect|yes|am flake-triage detect --help
help_verify_live|yes|am share deploy verify-live --help
help_share_wizard|no|am share wizard --help
EOF

# ===========================================================================
# Case 2: Invalid-flag remediation consistency
# ===========================================================================
e2e_case_banner "Invalid-flag remediation consistency"

while IFS='|' read -r case_id cmdline; do
    [ -n "${case_id}" ] || continue
    # shellcheck disable=SC2206
    cmd=( ${cmdline} )
    run_case "${case_id}" "${cmd[@]}"

    e2e_assert_exit_code "${case_id}: invalid flag exits 2" "2" "${CASE_RC}"
    e2e_assert_contains "${case_id}: error prefix present" "${CASE_STDERR}" "error:"
    e2e_assert_contains "${case_id}: remediation hints --help" "${CASE_STDERR}" "For more information, try '--help'."
done <<'EOF'
invalid_ci|am ci --definitely-invalid-flag
invalid_flake_scan|am flake-triage scan --definitely-invalid-flag
invalid_verify_live|am share deploy verify-live --definitely-invalid-flag
invalid_share_wizard|am share wizard --definitely-invalid-flag
EOF

# ===========================================================================
# Case 3: Onboarding smoke - first-run flake scan (empty dir)
# ===========================================================================
e2e_case_banner "Onboarding smoke: first-run flake scan"

EMPTY_SCAN_DIR="${WORK}/empty_scan_dir"
mkdir -p "${EMPTY_SCAN_DIR}"
run_case "onboard_flake_scan_empty" \
    am flake-triage scan --dir "${EMPTY_SCAN_DIR}" --json

e2e_assert_exit_code "flake scan empty dir exits 0" "0" "${CASE_RC}"
assert_valid_json "flake scan empty dir emits valid JSON" "${CASE_DIR}/stdout.txt"
if [ "${CASE_STDOUT}" = "[]" ]; then
    e2e_pass "flake scan empty dir returns empty list"
else
    add_finding "warning" "onboarding_smoke" "${CASE_ID}" \
        "Expected empty artifact list for empty scan dir, got non-empty output." \
        "Verify scan defaults and artifact filtering behavior."
fi

# ===========================================================================
# Case 4: Onboarding smoke - common failure path (missing artifact)
# ===========================================================================
e2e_case_banner "Onboarding smoke: flake reproduce missing artifact"

MISSING_ARTIFACT="${WORK}/missing_failure_context.json"
run_case "onboard_flake_reproduce_missing" \
    am flake-triage reproduce "${MISSING_ARTIFACT}"

e2e_assert_exit_code "flake reproduce missing artifact exits 1" "1" "${CASE_RC}"
e2e_assert_contains "flake reproduce missing artifact reports io failure" "${CASE_STDERR}" "error:"

if [[ "${CASE_STDERR}" == *"For more information, try '--help'."* ]]; then
    e2e_pass "flake reproduce missing artifact includes remediation hint"
else
    add_finding "warning" "remediation_guidance" "${CASE_ID}" \
        "Missing-artifact path returns raw IO error without explicit next-step guidance." \
        "Add a remediation hint (for example: verify path, rerun with --help, provide sample artifact location)."
fi

# ===========================================================================
# Case 5: Onboarding smoke - verify-live failure diagnostics are structured
# ===========================================================================
e2e_case_banner "Onboarding smoke: verify-live unreachable target diagnostics"

run_case "onboard_verify_live_unreachable" \
    am share deploy verify-live http://127.0.0.1:1 --json --timeout 100 --retries 0

e2e_assert_exit_code "verify-live unreachable target exits 1" "1" "${CASE_RC}"
assert_valid_json "verify-live unreachable emits valid JSON diagnostics" "${CASE_DIR}/stdout.txt"

python3 - <<'PY' "${CASE_DIR}/stdout.txt" > "${CASE_DIR}/diagnostics_check.txt"
import json
import sys

payload = json.load(open(sys.argv[1], "r", encoding="utf-8"))
verdict = payload.get("verdict")
summary = payload.get("summary") or {}
failed = summary.get("failed", 0)
config = payload.get("config") or {}
timeout_ms = config.get("timeout_ms")
print(f"verdict={verdict}")
print(f"failed={failed}")
print(f"timeout_ms={timeout_ms}")
PY

VERIFY_CHECK="$(cat "${CASE_DIR}/diagnostics_check.txt")"
e2e_assert_contains "verify-live verdict is fail" "${VERIFY_CHECK}" "verdict=fail"
e2e_assert_contains "verify-live reports failing checks" "${VERIFY_CHECK}" "failed="
e2e_assert_contains "verify-live preserves timeout config in report" "${VERIFY_CHECK}" "timeout_ms=100"

# ---------------------------------------------------------------------------
# Consolidated machine-readable report for this suite
# ---------------------------------------------------------------------------

python3 - <<'PY' \
    "${E2E_ARTIFACT_DIR}" "${FINDINGS_FILE}" "${E2E_SUITE}" "${E2E_RUN_STARTED_AT}" \
    > "${WORK}/ux_consistency_report.json"
import glob
import json
import os
import sys

artifact_dir = sys.argv[1]
findings_file = sys.argv[2]
suite = sys.argv[3]
run_started_at = sys.argv[4]

cases = []
for result_path in sorted(glob.glob(os.path.join(artifact_dir, "*", "result.json"))):
    with open(result_path, "r", encoding="utf-8") as fh:
        cases.append(json.load(fh))

findings = []
if os.path.exists(findings_file):
    with open(findings_file, "r", encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if not line:
                continue
            findings.append(json.loads(line))

summary = {
    "schema_version": "migration_ux_smoke.v1",
    "suite": suite,
    "run_started_at": run_started_at,
    "case_count": len(cases),
    "failure_case_count": sum(1 for c in cases if c.get("exit_code", 0) != 0),
    "finding_counts": {
        "error": sum(1 for f in findings if f.get("severity") == "error"),
        "warning": sum(1 for f in findings if f.get("severity") == "warning"),
    },
    "cases": cases,
    "findings": findings,
}
print(json.dumps(summary, indent=2))
PY

e2e_save_artifact "ux_consistency_report.json" "$(cat "${WORK}/ux_consistency_report.json")"

if [ -s "${FINDINGS_FILE}" ]; then
    e2e_save_artifact "ux_findings.jsonl" "$(cat "${FINDINGS_FILE}")"
    e2e_log "Findings recorded in ux_consistency_report.json and ux_findings.jsonl"
else
    e2e_log "No UX consistency findings recorded"
fi

e2e_summary

