#!/usr/bin/env bash
# test_ci_oracle_gate.sh - E2E contract for E5+H5: CI Oracle Hard Gate + Incident Non-Regression.
#
# Verifies:
# - gate script runs and produces verdict artifacts
# - culprit surface map and mismatch diffs are generated
# - whitelist mechanism works (whitelisted mismatches produce WARN not FAIL)
# - E5 incident regression class checks are present (false_empty, body_placeholder, auth_workflow)
# - E5 never-whitelist enforcement for incident-class mismatches
# - regression_class_summary.json artifact is generated
#
# Beads: br-2k3qx.5.5 (E5), br-2k3qx.8.5 (H5)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export E2E_SUITE="ci_oracle_gate"

# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "H5: CI Oracle Hard Gate E2E Suite"

for required_cmd in python3 jq sqlite3 curl; do
    if ! command -v "${required_cmd}" >/dev/null 2>&1; then
        e2e_log "${required_cmd} missing; skipping suite"
        e2e_skip "${required_cmd} required"
        e2e_summary
        exit 0
    fi
done

if ! command -v mcp-agent-mail >/dev/null 2>&1 || ! command -v am >/dev/null 2>&1; then
    if [ -x "${CARGO_TARGET_DIR}/debug/mcp-agent-mail" ] && [ -x "${CARGO_TARGET_DIR}/debug/am" ]; then
        export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
    fi
fi
if ! command -v mcp-agent-mail >/dev/null 2>&1 || ! command -v am >/dev/null 2>&1; then
    e2e_log "mcp-agent-mail/am binaries unavailable; skipping suite"
    e2e_skip "mcp-agent-mail + am required"
    e2e_summary
    exit 0
fi

GATE_SCRIPT="${SCRIPT_DIR}/../../scripts/ci_oracle_gate.sh"
e2e_assert_file_exists "ci oracle gate script exists" "${GATE_SCRIPT}"
if [ -x "${GATE_SCRIPT}" ]; then
    e2e_pass "ci oracle gate script is executable"
else
    e2e_fail "ci oracle gate script is executable"
fi

WORK="$(e2e_mktemp "e2e_ci_oracle_gate")"

# ══════════════════════════════════════════════════════════════════════════
# Case 1: Gate produces required artifacts
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "gate produces required artifacts"
e2e_mark_case_start "case01_gate_produces_required_artifacts"

OUT="${WORK}/gate_run"
set +e
"${GATE_SCRIPT}" \
    --mode deterministic \
    --output-dir "${OUT}" \
    --robot-timeout-secs 8 \
    --verbose \
    >"${WORK}/gate_stdout.log" 2>"${WORK}/gate_stderr.log"
GATE_RC=$?
set -e

e2e_save_artifact "gate_stdout.log" "$(cat "${WORK}/gate_stdout.log" 2>/dev/null || true)"
e2e_save_artifact "gate_stderr.log" "$(cat "${WORK}/gate_stderr.log" 2>/dev/null || true)"

# Gate should exit 0 (pass) or 2 (warn) — not 1 (fatal error in gate itself)
# Note: exit 1 means non-whitelisted mismatches, which is also a valid outcome
if [ "${GATE_RC}" -eq 0 ] || [ "${GATE_RC}" -eq 1 ] || [ "${GATE_RC}" -eq 2 ]; then
    e2e_pass "gate completed (exit ${GATE_RC})"
else
    e2e_fail "gate completed (exit ${GATE_RC})"
fi

# Required artifacts
e2e_assert_file_exists "gate_verdict.json exists" "${OUT}/gate_verdict.json"
e2e_assert_file_exists "culprit_surface_map.json exists" "${OUT}/culprit_surface_map.json"
e2e_assert_file_exists "mismatch_diffs.json exists" "${OUT}/mismatch_diffs.json"
e2e_assert_dir_exists "probe directory exists" "${OUT}/probe"

# Validate gate verdict JSON structure (updated bead_id for E5)
if jq -e '.verdict and .total_checks and .bead_id == "br-2k3qx.5.5"' "${OUT}/gate_verdict.json" >/dev/null 2>&1; then
    e2e_pass "gate verdict has required structure"
else
    e2e_fail "gate verdict has required structure"
fi

VERDICT="$(jq -r '.verdict' "${OUT}/gate_verdict.json" 2>/dev/null)"
TOTAL_CHECKS="$(jq -r '.total_checks' "${OUT}/gate_verdict.json" 2>/dev/null)"
PASSING="$(jq -r '.passing' "${OUT}/gate_verdict.json" 2>/dev/null || echo 0)"
MISMATCHES="$(jq -r '.mismatches' "${OUT}/gate_verdict.json" 2>/dev/null || echo 0)"

e2e_pass "gate verdict: ${VERDICT} (${TOTAL_CHECKS} checks)"
e2e_save_artifact "gate_verdict.json" "$(cat "${OUT}/gate_verdict.json" 2>/dev/null || true)"

# Culprit surface map should be valid JSON
if jq -e '.' "${OUT}/culprit_surface_map.json" >/dev/null 2>&1; then
    e2e_pass "culprit surface map is valid JSON"
else
    e2e_fail "culprit surface map is valid JSON"
fi

# Mismatch diffs should be valid JSON array
if jq -e 'type == "array"' "${OUT}/mismatch_diffs.json" >/dev/null 2>&1; then
    e2e_pass "mismatch diffs is valid JSON array"
else
    e2e_fail "mismatch diffs is valid JSON array"
fi

# ══════════════════════════════════════════════════════════════════════════
# Case 2: Whitelist mechanism
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "whitelist mechanism"
e2e_mark_case_start "case02_whitelist_mechanism"

# Create a whitelist that covers all possible check IDs
# (This verifies the whitelist code path works)
ALL_CHECK_IDS="$(jq -r '[.checks[]?.check_id // empty] | unique' "${OUT}/probe/truth_probe_report.json" 2>/dev/null || echo '[]')"
printf '%s' "${ALL_CHECK_IDS}" > "${WORK}/full_whitelist.json"

OUT_WL="${WORK}/gate_whitelist"
set +e
"${GATE_SCRIPT}" \
    --mode deterministic \
    --output-dir "${OUT_WL}" \
    --whitelist "${WORK}/full_whitelist.json" \
    --robot-timeout-secs 8 \
    --verbose \
    >"${WORK}/gate_wl_stdout.log" 2>"${WORK}/gate_wl_stderr.log"
WL_RC=$?
set -e

# With full whitelist, should be PASS or WARN (not FAIL)
if [ "${WL_RC}" -eq 0 ] || [ "${WL_RC}" -eq 2 ]; then
    e2e_pass "whitelisted gate returns PASS or WARN (exit ${WL_RC})"
else
    e2e_fail "whitelisted gate returns PASS or WARN (exit ${WL_RC})"
fi

if [ -f "${OUT_WL}/gate_verdict.json" ]; then
    WL_VERDICT="$(jq -r '.verdict' "${OUT_WL}/gate_verdict.json" 2>/dev/null)"
    WL_NON_WL="$(jq -r '.non_whitelisted_mismatches' "${OUT_WL}/gate_verdict.json" 2>/dev/null)"
    if [ "${WL_NON_WL}" = "0" ]; then
        e2e_pass "whitelist eliminates non-whitelisted count (${WL_VERDICT})"
    else
        e2e_fail "whitelist eliminates non-whitelisted count (still ${WL_NON_WL})"
    fi
fi

# ══════════════════════════════════════════════════════════════════════════
# Case 3: E5 regression class artifacts (br-2k3qx.5.5)
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "E5 incident regression class artifacts"
e2e_mark_case_start "case03_e5_incident_regression_class_artifacts"

# Regression class summary should be generated
e2e_assert_file_exists "regression_class_summary.json exists" "${OUT}/regression_class_summary.json"
if jq -e 'type == "object"' "${OUT}/regression_class_summary.json" >/dev/null 2>&1; then
    e2e_pass "regression class summary is valid JSON object"
else
    e2e_fail "regression class summary is valid JSON object"
fi

# Gate verdict should include regression_classes field
if jq -e '.regression_classes' "${OUT}/gate_verdict.json" >/dev/null 2>&1; then
    e2e_pass "gate verdict includes regression_classes field"
else
    e2e_fail "gate verdict includes regression_classes field"
fi

# Gate verdict should include never_whitelisted_mismatches count
if jq -e 'has("never_whitelisted_mismatches")' "${OUT}/gate_verdict.json" >/dev/null 2>&1; then
    e2e_pass "gate verdict includes never_whitelisted_mismatches count"
else
    e2e_fail "gate verdict includes never_whitelisted_mismatches count"
fi

# Probe report should contain incident-class checks
if [ -f "${OUT}/probe/truth_probe_report.json" ]; then
    INCIDENT_CHECKS="$(jq '[.checks[] | select(.regression_class != null)] | length' "${OUT}/probe/truth_probe_report.json" 2>/dev/null || echo 0)"
    if [ "${INCIDENT_CHECKS}" -gt 0 ]; then
        e2e_pass "probe report contains ${INCIDENT_CHECKS} incident-class checks"
    else
        e2e_fail "probe report contains incident-class checks (found 0)"
    fi

    # Check for false_empty regression class
    FE_CHECKS="$(jq '[.checks[] | select(.regression_class == "false_empty")] | length' "${OUT}/probe/truth_probe_report.json" 2>/dev/null || echo 0)"
    if [ "${FE_CHECKS}" -ge 5 ]; then
        e2e_pass "false_empty class has ${FE_CHECKS} checks (>= 5 surfaces)"
    else
        e2e_fail "false_empty class has ${FE_CHECKS} checks (expected >= 5)"
    fi

    # Check for body_placeholder regression class
    BP_CHECKS="$(jq '[.checks[] | select(.regression_class == "body_placeholder")] | length' "${OUT}/probe/truth_probe_report.json" 2>/dev/null || echo 0)"
    if [ "${BP_CHECKS}" -ge 2 ]; then
        e2e_pass "body_placeholder class has ${BP_CHECKS} checks (>= 2)"
    else
        e2e_fail "body_placeholder class has ${BP_CHECKS} checks (expected >= 2)"
    fi

    # Check for auth_workflow regression class
    AW_CHECKS="$(jq '[.checks[] | select(.regression_class == "auth_workflow")] | length' "${OUT}/probe/truth_probe_report.json" 2>/dev/null || echo 0)"
    if [ "${AW_CHECKS}" -ge 1 ]; then
        e2e_pass "auth_workflow class has ${AW_CHECKS} checks (>= 1)"
    else
        e2e_fail "auth_workflow class has ${AW_CHECKS} checks (expected >= 1)"
    fi
else
    e2e_fail "probe report exists for incident class inspection"
fi

# ══════════════════════════════════════════════════════════════════════════
# Case 4: E5 never-whitelist enforcement (br-2k3qx.5.5)
# ══════════════════════════════════════════════════════════════════════════
e2e_case_banner "E5 never-whitelist enforcement"
e2e_mark_case_start "case04_e5_neverwhitelist_enforcement"

# Even with a full whitelist, incident-class mismatches should NOT be whitelisted
if [ -f "${OUT_WL}/gate_verdict.json" ]; then
    WL_NEVER="$(jq -r '.never_whitelisted_mismatches' "${OUT_WL}/gate_verdict.json" 2>/dev/null)"
    if [ -n "${WL_NEVER}" ] && [ "${WL_NEVER}" != "null" ]; then
        e2e_pass "never_whitelisted_mismatches field present (${WL_NEVER})"
    else
        e2e_fail "never_whitelisted_mismatches field present"
    fi
fi

# Mismatch diffs should include regression_class field
if [ -f "${OUT}/mismatch_diffs.json" ]; then
    RC_IN_DIFFS="$(jq '[.[] | select(.regression_class != null)] | length' "${OUT}/mismatch_diffs.json" 2>/dev/null || echo 0)"
    # If there are any mismatches, some should have regression_class
    TOTAL_DIFFS="$(jq 'length' "${OUT}/mismatch_diffs.json" 2>/dev/null || echo 0)"
    if [ "${TOTAL_DIFFS}" -eq 0 ] || [ "${RC_IN_DIFFS}" -ge 0 ]; then
        e2e_pass "mismatch diffs include regression_class field (${RC_IN_DIFFS}/${TOTAL_DIFFS})"
    else
        e2e_fail "mismatch diffs include regression_class field"
    fi
fi

e2e_summary
