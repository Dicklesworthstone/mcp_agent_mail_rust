#!/usr/bin/env bash
# test_t16_workflow_gate.sh - Umbrella validation gate for T16 E2E workflows (br-1xt0m.1.13.3)
#
# Validates that all prerequisite T16 E2E suites exist and the orchestration
# entrypoint is properly configured. This is an umbrella gate bead whose
# acceptance criterion is met when all blocking beads (1.13.10-15) are closed
# and their test suites are present and structurally sound.
#
# Checked:
#   - t16_validate.sh orchestrator exists and is runnable
#   - All expected T16 E2E suite scripts exist
#   - Each suite sources e2e_lib.sh and sets E2E_SUITE
#   - Orchestrator --dry-run produces all expected phases
#   - Logging contract suite validates independently (lightweight check)
#   - Artifact schema suite validates independently (lightweight check)

E2E_SUITE="t16_workflow_gate"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "T16 E2E Workflow Gate (br-1xt0m.1.13.3)"

PROJECT_ROOT="${E2E_PROJECT_ROOT:-$(cd "${SCRIPT_DIR}/../.." && pwd)}"
T16_SCRIPT="${PROJECT_ROOT}/scripts/t16_validate.sh"
SUITES_DIR="${PROJECT_ROOT}/tests/e2e"

# ── Case 1: Orchestrator exists and runs ────────────────────────────

e2e_case_banner "Orchestrator entrypoint"

if [ -f "$T16_SCRIPT" ] && [ -x "$T16_SCRIPT" ]; then
    e2e_pass "t16_validate.sh exists and is executable"
else
    e2e_fail "t16_validate.sh not found or not executable at ${T16_SCRIPT}"
fi

# Verify --list works.
set +e
LIST_OUTPUT="$(bash "$T16_SCRIPT" --list 2>&1)"
list_rc=$?
set -e

if [ "$list_rc" -eq 0 ]; then
    e2e_pass "t16_validate.sh --list exits cleanly"
else
    e2e_fail "t16_validate.sh --list failed (rc=${list_rc})"
fi

# ── Case 2: All T16 E2E suite scripts exist ─────────────────────────

e2e_case_banner "T16 E2E suite inventory"

# These are the suites required by the T16 orchestrator (Phase 4 + 5).
REQUIRED_SUITES=(
    "test_logging_contract.sh:br-1xt0m.1.13.13"
    "test_failure_injection.sh:br-1xt0m.1.13.15"
    "test_stdio_workflows.sh:br-1xt0m.1.13.10"
    "test_stdio_screen_workflows.sh:br-1xt0m.1.13.11"
    "test_stdio_adaptive.sh:br-1xt0m.1.13.12"
    "test_artifacts_schema.sh:harness"
    "test_perf_regression.sh:br-1xt0m.1.13.14"
)

all_present=true
for entry in "${REQUIRED_SUITES[@]}"; do
    script="${entry%%:*}"
    bead="${entry##*:}"
    if [ -f "${SUITES_DIR}/${script}" ]; then
        :
    else
        e2e_fail "missing suite: ${script} (${bead})"
        all_present=false
    fi
done

if [ "$all_present" = "true" ]; then
    e2e_pass "all ${#REQUIRED_SUITES[@]} required T16 suite scripts present"
fi

# ── Case 3: Suite structural validation ──────────────────────────────

e2e_case_banner "Suite structural validation"

struct_ok=0
struct_fail=0
for entry in "${REQUIRED_SUITES[@]}"; do
    script="${entry%%:*}"
    suite_file="${SUITES_DIR}/${script}"
    [ -f "$suite_file" ] || continue

    # Must source e2e_lib.sh
    if grep -q 'e2e_lib.sh' "$suite_file" 2>/dev/null; then
        :
    else
        e2e_fail "${script}: does not source e2e_lib.sh"
        (( struct_fail++ )) || true
        continue
    fi

    # Must set E2E_SUITE
    if grep -q 'E2E_SUITE=' "$suite_file" 2>/dev/null; then
        (( struct_ok++ )) || true
    else
        e2e_fail "${script}: does not set E2E_SUITE"
        (( struct_fail++ )) || true
    fi
done

if [ "$struct_fail" -eq 0 ] && [ "$struct_ok" -gt 0 ]; then
    e2e_pass "all ${struct_ok} suites structurally valid (e2e_lib + E2E_SUITE)"
fi

# ── Case 4: Orchestrator dry-run produces all phases ─────────────────

e2e_case_banner "Orchestrator phase plan"

DRY_OUTPUT="${E2E_ARTIFACT_DIR}/dry_run.log"
set +e
bash "$T16_SCRIPT" --dry-run >"$DRY_OUTPUT" 2>&1
dry_rc=$?
set -e

if [ "$dry_rc" -eq 0 ]; then
    e2e_pass "orchestrator --dry-run exits cleanly"
else
    e2e_fail "orchestrator --dry-run failed (rc=${dry_rc})"
fi

e2e_save_artifact "dry_run.log" "$(cat "$DRY_OUTPUT" 2>/dev/null)"

# Verify expected phases appear in dry-run output.
EXPECTED_PHASES=(
    "Build Verification"
    "Rust Unit Tests"
    "Snapshot and Integration"
    "E2E Shell Suites"
    "Performance Regression"
    "Evidence Summary"
)

missing_phases=()
for phase in "${EXPECTED_PHASES[@]}"; do
    if grep -q "$phase" "$DRY_OUTPUT" 2>/dev/null; then
        :
    else
        missing_phases+=("$phase")
    fi
done

if [ ${#missing_phases[@]} -eq 0 ]; then
    e2e_pass "all ${#EXPECTED_PHASES[@]} phases present in dry-run plan"
else
    e2e_fail "missing phases: ${missing_phases[*]}"
fi

# ── Case 5: Lightweight functional checks ────────────────────────────

e2e_case_banner "Lightweight functional checks"

# Run two fast suites to verify the harness works end-to-end.
# logging_contract is ~2 seconds, artifacts_schema is ~1 second.

e2e_step_start "logging_contract_check"
set +e
bash "${SUITES_DIR}/test_logging_contract.sh" >"${E2E_ARTIFACT_DIR}/logging_contract.log" 2>&1
lc_rc=$?
set -e
e2e_step_end "logging_contract_check"

if [ "$lc_rc" -eq 0 ]; then
    e2e_pass "logging contract suite passes"
else
    e2e_fail "logging contract suite failed (rc=${lc_rc})"
fi

e2e_step_start "artifacts_schema_check"
set +e
bash "${SUITES_DIR}/test_artifacts_schema.sh" >"${E2E_ARTIFACT_DIR}/artifacts_schema.log" 2>&1
as_rc=$?
set -e
e2e_step_end "artifacts_schema_check"

if [ "$as_rc" -eq 0 ]; then
    e2e_pass "artifacts schema suite passes"
else
    e2e_fail "artifacts schema suite failed (rc=${as_rc})"
fi

# ── Summary ──────────────────────────────────────────────────────────

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
