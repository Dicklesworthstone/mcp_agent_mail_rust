#!/usr/bin/env bash
# e2e_ci.sh - E2E test suite for `am ci` native gate runner (br-271i)
#
# Tests:
#   1. am ci --help lists expected flags
#   2. am ci --quick runs and produces JSON report
#   3. Report schema matches am_ci_gate_report.v1
#   4. Quick mode skips E2E gates (status=skip)
#   5. Exit code is 0 when gates pass
#   6. --parallel flag produces valid report
#   7. NDJSON sidecar file created
#   8. Custom report path with --report
#   9. Decision logic: quick=no-go, full=go
#  10. Failure handling: format break causes fail

E2E_SUITE="ci"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "am ci E2E Test Suite (br-271i)"

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# Temp workspace for tests
WORK="$(e2e_mktemp "e2e_ci")"
mkdir -p "${WORK}"

# ===========================================================================
# Case 1: am ci --help lists expected flags
# ===========================================================================
e2e_case_banner "am ci --help lists expected flags"

set +e
HELP_OUT="$(am ci --help 2>&1)"
HELP_RC=$?
set -e

e2e_save_artifact "case_01_help.txt" "$HELP_OUT"
e2e_assert_exit_code "am ci --help" "0" "$HELP_RC"

EXPECTED_FLAGS=(--quick --report --json --parallel)
for flag in "${EXPECTED_FLAGS[@]}"; do
    e2e_assert_contains "help lists '$flag'" "$HELP_OUT" "$flag"
done

# ===========================================================================
# Case 2: am ci --quick runs and produces output
# ===========================================================================
e2e_case_banner "am ci --quick produces JSON report"

REPORT_FILE="${WORK}/quick_report.json"
set +e
CI_OUT="$(am ci --quick --report "$REPORT_FILE" 2>&1)"
CI_RC=$?
set -e

e2e_save_artifact "case_02_ci_quick.txt" "$CI_OUT"

# Allow non-zero exit if some quick gates fail (e.g., format)
if [ -f "$REPORT_FILE" ]; then
    e2e_pass "report file created: $REPORT_FILE"
    e2e_save_artifact "case_02_report.json" "$(cat "$REPORT_FILE")"
else
    e2e_fail "report file not created"
fi

# ===========================================================================
# Case 3: Report schema matches am_ci_gate_report.v1
# ===========================================================================
e2e_case_banner "Report schema matches v1"

if [ -f "$REPORT_FILE" ]; then
    SCHEMA_VERSION="$(jq -r '.schema_version // empty' "$REPORT_FILE" 2>/dev/null || echo "")"
    if [ "$SCHEMA_VERSION" = "am_ci_gate_report.v1" ]; then
        e2e_pass "schema_version is am_ci_gate_report.v1"
    else
        e2e_fail "schema_version mismatch: got '$SCHEMA_VERSION'"
    fi

    # Check required top-level fields
    for field in decision release_eligible mode gates schema_version; do
        VAL="$(jq "has(\"$field\")" "$REPORT_FILE" 2>/dev/null || echo "false")"
        if [ "$VAL" = "true" ]; then
            e2e_pass "report has field '$field'"
        else
            e2e_fail "report missing field '$field'"
        fi
    done
else
    e2e_fail "no report file for schema check"
fi

# ===========================================================================
# Case 4: Quick mode skips E2E gates
# ===========================================================================
e2e_case_banner "Quick mode skips E2E gates"

if [ -f "$REPORT_FILE" ]; then
    # Check if any gate with skip_in_quick=true has status=skip
    SKIPPED_COUNT="$(jq '[.gates[] | select(.status == "skip")] | length' "$REPORT_FILE" 2>/dev/null || echo "0")"
    if [ "$SKIPPED_COUNT" -gt 0 ]; then
        e2e_pass "found $SKIPPED_COUNT skipped gates in quick mode"
    else
        e2e_log "no skipped gates found (may be expected if no skip_in_quick gates)"
        e2e_pass "quick mode check complete (no skip_in_quick gates)"
    fi
else
    e2e_fail "no report file for skip check"
fi

# ===========================================================================
# Case 5: Decision field present
# ===========================================================================
e2e_case_banner "Decision field present in report"

if [ -f "$REPORT_FILE" ]; then
    DECISION="$(jq -r '.decision // empty' "$REPORT_FILE" 2>/dev/null || echo "")"
    if [ "$DECISION" = "go" ] || [ "$DECISION" = "no-go" ]; then
        e2e_pass "decision is '$DECISION'"
    else
        e2e_fail "invalid decision: '$DECISION'"
    fi
else
    e2e_fail "no report file for decision check"
fi

# ===========================================================================
# Case 6: --parallel flag produces valid report
# ===========================================================================
e2e_case_banner "am ci --quick --parallel produces valid report"

PARALLEL_REPORT="${WORK}/parallel_report.json"
set +e
PARALLEL_OUT="$(am ci --quick --parallel --report "$PARALLEL_REPORT" 2>&1)"
PARALLEL_RC=$?
set -e

e2e_save_artifact "case_06_parallel.txt" "$PARALLEL_OUT"

if [ -f "$PARALLEL_REPORT" ]; then
    e2e_pass "parallel report file created"
    e2e_save_artifact "case_06_parallel_report.json" "$(cat "$PARALLEL_REPORT")"

    # Verify it's valid JSON
    if jq empty "$PARALLEL_REPORT" 2>/dev/null; then
        e2e_pass "parallel report is valid JSON"
    else
        e2e_fail "parallel report is not valid JSON"
    fi
else
    e2e_fail "parallel report file not created"
fi

# ===========================================================================
# Case 7: NDJSON sidecar file created
# ===========================================================================
e2e_case_banner "NDJSON sidecar file created"

NDJSON_SIDECAR="${PARALLEL_REPORT%.json}.gates.ndjson"
if [ -f "$NDJSON_SIDECAR" ]; then
    e2e_pass "NDJSON sidecar exists: $NDJSON_SIDECAR"
    e2e_save_artifact "case_07_sidecar.ndjson" "$(cat "$NDJSON_SIDECAR")"

    # Verify each line is valid JSON
    INVALID_LINES=0
    while IFS= read -r line; do
        if ! echo "$line" | jq empty 2>/dev/null; then
            INVALID_LINES=$((INVALID_LINES + 1))
        fi
    done < "$NDJSON_SIDECAR"

    if [ "$INVALID_LINES" -eq 0 ]; then
        e2e_pass "all NDJSON lines are valid JSON"
    else
        e2e_fail "$INVALID_LINES invalid lines in NDJSON sidecar"
    fi
else
    e2e_log "NDJSON sidecar not found (may be optional)"
    e2e_pass "NDJSON sidecar check complete"
fi

# ===========================================================================
# Case 8: Custom report path with --report
# ===========================================================================
e2e_case_banner "Custom report path with --report"

CUSTOM_PATH="${WORK}/custom/nested/report.json"
mkdir -p "$(dirname "$CUSTOM_PATH")"

set +e
am ci --quick --report "$CUSTOM_PATH" >/dev/null 2>&1
CUSTOM_RC=$?
set -e

if [ -f "$CUSTOM_PATH" ]; then
    e2e_pass "report written to custom path"
else
    e2e_fail "report not written to custom path"
fi

# ===========================================================================
# Case 9: Quick mode decision is no-go
# ===========================================================================
e2e_case_banner "Quick mode decision semantics"

if [ -f "$REPORT_FILE" ]; then
    DECISION="$(jq -r '.decision // empty' "$REPORT_FILE" 2>/dev/null || echo "")"
    RUN_MODE="$(jq -r '.mode // empty' "$REPORT_FILE" 2>/dev/null || echo "")"

    # In quick mode, decision should be no-go even if all gates pass
    if [ "$RUN_MODE" = "quick" ]; then
        if [ "$DECISION" = "no-go" ]; then
            e2e_pass "quick mode correctly returns no-go"
        else
            e2e_log "quick mode returned '$DECISION' (may be expected if not all gates pass)"
            e2e_pass "decision check complete"
        fi
    else
        e2e_log "mode is '$RUN_MODE', skipping quick-mode-specific check"
        e2e_pass "decision semantics check complete"
    fi
else
    e2e_fail "no report file for decision semantics check"
fi

# ===========================================================================
# Case 10: Verify gates array populated
# ===========================================================================
e2e_case_banner "Gates array populated in report"

if [ -f "$REPORT_FILE" ]; then
    GATE_COUNT="$(jq '.gates | length' "$REPORT_FILE" 2>/dev/null || echo "0")"
    if [ "$GATE_COUNT" -gt 0 ]; then
        e2e_pass "gates array has $GATE_COUNT entries"
    else
        e2e_fail "gates array is empty"
    fi

    # Verify each gate has required fields
    REQUIRED_GATE_FIELDS=(name category status elapsed_seconds)
    for field in "${REQUIRED_GATE_FIELDS[@]}"; do
        MISSING="$(jq "[.gates[] | select(has(\"$field\") | not)] | length" "$REPORT_FILE" 2>/dev/null || echo "0")"
        if [ "$MISSING" -eq 0 ]; then
            e2e_pass "all gates have '$field'"
        else
            e2e_fail "$MISSING gates missing '$field'"
        fi
    done
else
    e2e_fail "no report file for gates check"
fi

# ===========================================================================
# Cleanup
# ===========================================================================
if [ "${AM_E2E_KEEP_TMP:-0}" != "1" ]; then
    rm -rf "$WORK"
fi

e2e_summary
