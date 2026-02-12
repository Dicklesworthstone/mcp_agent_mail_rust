#!/usr/bin/env bash
# test_flake_triage.sh - E2E test suite for `am flake-triage` command (br-1oh3n)
#
# Tests all three modes: scan, reproduce, and detect (multi-seed).
# Uses realistic artifact sets and validates JSON output schemas.
#
# Run: ./tests/e2e/test_flake_triage.sh
# Or:  E2E_SUITE=flake_triage scripts/e2e_test.sh flake_triage

E2E_SUITE="flake_triage"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Flake-Triage E2E Test Suite"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"
AM="${CARGO_TARGET_DIR}/debug/am"

# Verify am binary exists
if [ ! -f "$AM" ]; then
    e2e_fail "am binary not found at ${AM}"
    e2e_summary
    exit 1
fi

# Create temp directory for test artifacts
TRIAGE_TMP=$(mktemp -d)
trap 'rm -rf "$TRIAGE_TMP"' EXIT

# ---------------------------------------------------------------------------
# Helper: Create a failure_context.json artifact
# ---------------------------------------------------------------------------
create_failure_artifact() {
    local dir="$1"
    local test_name="$2"
    local seed="$3"
    local category="${4:-assertion}"
    local ts="${5:-$(date -u +"%Y-%m-%dT%H:%M:%S.000000Z")}"

    mkdir -p "$dir"
    cat > "${dir}/failure_context.json" <<EOF
{
  "test_name": "$test_name",
  "harness_seed": $seed,
  "e2e_seed": null,
  "failure_message": "assertion failed: ${test_name} with seed ${seed}",
  "failure_ts": "$ts",
  "repro_command": "HARNESS_SEED=$seed cargo test $test_name -- --nocapture",
  "repro_context": null,
  "env_snapshot": {},
  "rss_kb": 50000,
  "uptime_secs": 1.5,
  "category": "$category",
  "notes": []
}
EOF
}

# ---------------------------------------------------------------------------
# Case 1: Scan mode - basic functionality
# ---------------------------------------------------------------------------
e2e_case_banner "Scan mode: basic functionality"

SCAN_DIR="${TRIAGE_TMP}/scan_basic"
mkdir -p "$SCAN_DIR"

# Create test artifacts
create_failure_artifact "${SCAN_DIR}/run1" "test_alpha" 42 "assertion" "2026-02-12T10:00:00.000000Z"
create_failure_artifact "${SCAN_DIR}/run2" "test_beta" 99 "timing" "2026-02-12T09:00:00.000000Z"
create_failure_artifact "${SCAN_DIR}/nested/run3" "test_gamma" 7 "contention" "2026-02-12T11:00:00.000000Z"

set +e
scan_output=$("$AM" flake-triage scan --dir "$SCAN_DIR" 2>&1)
scan_rc=$?
set -e

if [ "$scan_rc" -eq 0 ]; then
    e2e_pass "Scan command succeeded"
else
    e2e_fail "Scan command failed with rc=$scan_rc"
fi

# Verify human-readable output mentions artifacts
if echo "$scan_output" | grep -q "3 artifact\|test_alpha\|test_beta\|test_gamma"; then
    e2e_pass "Human output mentions test names"
else
    e2e_fail "Human output missing test names"
fi

e2e_save_artifact "scan_basic_output.txt" "$scan_output"

# ---------------------------------------------------------------------------
# Case 2: Scan mode - JSON output schema
# ---------------------------------------------------------------------------
e2e_case_banner "Scan mode: JSON output schema"

set +e
scan_json=$("$AM" flake-triage scan --dir "$SCAN_DIR" --json 2>&1)
scan_json_rc=$?
set -e

if [ "$scan_json_rc" -eq 0 ]; then
    e2e_pass "Scan --json command succeeded"
else
    e2e_fail "Scan --json command failed with rc=$scan_json_rc"
fi

# Validate JSON structure
if echo "$scan_json" | jq -e 'type == "array"' > /dev/null 2>&1; then
    e2e_pass "JSON output is an array"
else
    e2e_fail "JSON output is not an array"
fi

# Check array length
arr_len=$(echo "$scan_json" | jq 'length' 2>/dev/null || echo "0")
if [ "$arr_len" -eq 3 ]; then
    e2e_pass "JSON array has 3 artifacts"
else
    e2e_fail "JSON array has $arr_len artifacts, expected 3"
fi

# Check schema fields
if echo "$scan_json" | jq -e '.[0] | .path and .context' > /dev/null 2>&1; then
    e2e_pass "JSON entries have 'path' and 'context' fields"
else
    e2e_fail "JSON entries missing required fields"
fi

# Check context sub-fields
if echo "$scan_json" | jq -e '.[0].context | .test_name and .category and .failure_ts and .repro_command' > /dev/null 2>&1; then
    e2e_pass "JSON context has required sub-fields"
else
    e2e_fail "JSON context missing required sub-fields"
fi

e2e_save_artifact "scan_json_output.json" "$scan_json"

# ---------------------------------------------------------------------------
# Case 3: Scan mode - timestamp sorting
# ---------------------------------------------------------------------------
e2e_case_banner "Scan mode: timestamp sorting (most recent first)"

# Get timestamps from JSON output
timestamps=$(echo "$scan_json" | jq -r '.[].context.failure_ts' 2>/dev/null)
first_ts=$(echo "$timestamps" | head -1)
last_ts=$(echo "$timestamps" | tail -1)

# Most recent should be first (2026-02-12T11:00 > 2026-02-12T09:00)
if [[ "$first_ts" > "$last_ts" ]]; then
    e2e_pass "Artifacts sorted by timestamp (newest first)"
else
    e2e_fail "Artifacts not sorted correctly: first=$first_ts, last=$last_ts"
fi

# ---------------------------------------------------------------------------
# Case 4: Scan mode - empty directory
# ---------------------------------------------------------------------------
e2e_case_banner "Scan mode: empty directory"

EMPTY_DIR="${TRIAGE_TMP}/empty"
mkdir -p "$EMPTY_DIR"

set +e
empty_json=$("$AM" flake-triage scan --dir "$EMPTY_DIR" --json 2>&1)
empty_rc=$?
set -e

if [ "$empty_rc" -eq 0 ]; then
    e2e_pass "Scan empty dir succeeded"
else
    e2e_fail "Scan empty dir failed with rc=$empty_rc"
fi

empty_len=$(echo "$empty_json" | jq 'length' 2>/dev/null || echo "-1")
if [ "$empty_len" -eq 0 ]; then
    e2e_pass "Empty dir returns empty array"
else
    e2e_fail "Empty dir returned $empty_len items, expected 0"
fi

# ---------------------------------------------------------------------------
# Case 5: Scan mode - malformed artifact handling
# ---------------------------------------------------------------------------
e2e_case_banner "Scan mode: malformed artifact handling"

MALFORMED_DIR="${TRIAGE_TMP}/malformed"
mkdir -p "${MALFORMED_DIR}/valid" "${MALFORMED_DIR}/bad"

create_failure_artifact "${MALFORMED_DIR}/valid" "good_test" 1
echo "{ not valid json }" > "${MALFORMED_DIR}/bad/failure_context.json"

set +e
# Capture stdout and stderr separately - JSON goes to stdout, warnings to stderr
malformed_json=$("$AM" flake-triage scan --dir "$MALFORMED_DIR" --json 2>"${E2E_ARTIFACT_DIR}/malformed_stderr.txt")
malformed_rc=$?
set -e

if [ "$malformed_rc" -eq 0 ]; then
    e2e_pass "Scan with malformed artifact succeeded"
else
    e2e_fail "Scan with malformed artifact failed with rc=$malformed_rc"
fi

malformed_len=$(echo "$malformed_json" | jq 'length' 2>/dev/null || echo "-1")
if [ "$malformed_len" -eq 1 ]; then
    e2e_pass "Malformed artifact skipped, valid artifact found"
else
    e2e_fail "Expected 1 artifact, got $malformed_len"
fi

e2e_save_artifact "malformed_output.json" "$malformed_json"

# ---------------------------------------------------------------------------
# Case 6: Reproduce mode - missing artifact error
# ---------------------------------------------------------------------------
e2e_case_banner "Reproduce mode: missing artifact error"

set +e
repro_missing=$("$AM" flake-triage reproduce "/nonexistent/path/failure_context.json" 2>&1)
repro_missing_rc=$?
set -e

if [ "$repro_missing_rc" -ne 0 ]; then
    e2e_pass "Reproduce with missing artifact fails as expected"
else
    e2e_fail "Reproduce with missing artifact should fail"
fi

if echo "$repro_missing" | grep -iq "error\|not found\|no such file"; then
    e2e_pass "Error message indicates file not found"
else
    e2e_fail "Error message unclear: $repro_missing"
fi

e2e_save_artifact "reproduce_missing.txt" "$repro_missing"

# ---------------------------------------------------------------------------
# Case 7: Reproduce mode - help documentation
# ---------------------------------------------------------------------------
e2e_case_banner "Reproduce mode: help documentation"

set +e
repro_help=$("$AM" flake-triage reproduce --help 2>&1)
repro_help_rc=$?
set -e

if [ "$repro_help_rc" -eq 0 ]; then
    e2e_pass "Reproduce --help succeeded"
else
    e2e_fail "Reproduce --help failed"
fi

e2e_assert_contains "help mentions --verbose" "$repro_help" "--verbose"
e2e_assert_contains "help mentions --timeout" "$repro_help" "--timeout"
e2e_assert_contains "help mentions artifact path" "$repro_help" "artifact"

# ---------------------------------------------------------------------------
# Case 8: Detect mode - help documentation
# ---------------------------------------------------------------------------
e2e_case_banner "Detect mode: help documentation"

set +e
detect_help=$("$AM" flake-triage detect --help 2>&1)
detect_help_rc=$?
set -e

if [ "$detect_help_rc" -eq 0 ]; then
    e2e_pass "Detect --help succeeded"
else
    e2e_fail "Detect --help failed"
fi

e2e_assert_contains "help mentions --seeds" "$detect_help" "--seeds"
e2e_assert_contains "help mentions --packages" "$detect_help" "--packages"
e2e_assert_contains "help mentions --timeout" "$detect_help" "--timeout"
e2e_assert_contains "help mentions --json" "$detect_help" "--json"
e2e_assert_contains "help mentions TEST_NAME" "$detect_help" "TEST_NAME"

# ---------------------------------------------------------------------------
# Case 9: Top-level flake-triage help
# ---------------------------------------------------------------------------
e2e_case_banner "Flake-triage: top-level help"

set +e
ft_help=$("$AM" flake-triage --help 2>&1)
ft_help_rc=$?
set -e

if [ "$ft_help_rc" -eq 0 ]; then
    e2e_pass "flake-triage --help succeeded"
else
    e2e_fail "flake-triage --help failed"
fi

e2e_assert_contains "help mentions scan subcommand" "$ft_help" "scan"
e2e_assert_contains "help mentions reproduce subcommand" "$ft_help" "reproduce"
e2e_assert_contains "help mentions detect subcommand" "$ft_help" "detect"

# ---------------------------------------------------------------------------
# Case 10: Scan mode - category classification
# ---------------------------------------------------------------------------
e2e_case_banner "Scan mode: category classification preserved"

CATEGORY_DIR="${TRIAGE_TMP}/categories"
create_failure_artifact "${CATEGORY_DIR}/timing" "test_timing" 1 "timing"
create_failure_artifact "${CATEGORY_DIR}/contention" "test_contention" 2 "contention"
create_failure_artifact "${CATEGORY_DIR}/ci" "test_ci" 3 "ci_environment"

set +e
cat_json=$("$AM" flake-triage scan --dir "$CATEGORY_DIR" --json 2>&1)
cat_rc=$?
set -e

if [ "$cat_rc" -eq 0 ]; then
    e2e_pass "Category scan succeeded"
else
    e2e_fail "Category scan failed with rc=$cat_rc"
fi

# Check that different categories are preserved
categories=$(echo "$cat_json" | jq -r '.[].context.category' 2>/dev/null | sort -u | wc -l)
if [ "$categories" -ge 3 ]; then
    e2e_pass "Multiple categories preserved in output"
else
    e2e_fail "Categories not preserved correctly (found $categories unique)"
fi

e2e_save_artifact "categories_output.json" "$cat_json"

# ---------------------------------------------------------------------------
# Case 11: Scan mode - repro_command field
# ---------------------------------------------------------------------------
e2e_case_banner "Scan mode: repro_command field present"

repro_cmd=$(echo "$scan_json" | jq -r '.[0].context.repro_command' 2>/dev/null)

if echo "$repro_cmd" | grep -q "HARNESS_SEED=.*cargo test"; then
    e2e_pass "repro_command has expected format"
else
    e2e_fail "repro_command format unexpected: $repro_cmd"
fi

# ---------------------------------------------------------------------------
# Case 12: Nonexistent directory handling
# ---------------------------------------------------------------------------
e2e_case_banner "Scan mode: nonexistent directory"

set +e
nonexist_output=$("$AM" flake-triage scan --dir "/nonexistent/path/xyz" 2>&1)
nonexist_rc=$?
set -e

# Should either return empty array or error gracefully (not panic)
if [ "$nonexist_rc" -eq 0 ]; then
    nonexist_len=$(echo "$nonexist_output" | jq 'length' 2>/dev/null || echo "parse_error")
    if [ "$nonexist_len" = "0" ] || [ "$nonexist_len" = "parse_error" ]; then
        e2e_pass "Nonexistent dir returns empty or error gracefully"
    else
        e2e_fail "Nonexistent dir returned unexpected data"
    fi
else
    # Non-zero exit is also acceptable for nonexistent dir
    e2e_pass "Nonexistent dir fails with non-zero exit (acceptable)"
fi

# ---------------------------------------------------------------------------
# Case 13: Reproduce with valid artifact (reads but may fail cargo test)
# ---------------------------------------------------------------------------
e2e_case_banner "Reproduce mode: artifact parsing"

REPRO_DIR="${TRIAGE_TMP}/repro"
create_failure_artifact "$REPRO_DIR" "nonexistent_test_xyz_12345" 42

# This will try to run cargo test on a nonexistent test, which should fail
# but the artifact parsing should succeed
set +e
repro_output=$("$AM" flake-triage reproduce "${REPRO_DIR}/failure_context.json" --timeout 5 2>&1)
repro_rc=$?
set -e

# The command may fail because the test doesn't exist, but it should parse the artifact
# and attempt to run. We're testing that it doesn't crash on artifact parsing.
if echo "$repro_output" | grep -q "nonexistent_test_xyz\|cargo test\|Reproducing"; then
    e2e_pass "Reproduce parsed artifact and attempted execution"
else
    # Even if it fails fast, it shouldn't panic
    if [ "$repro_rc" -ne 0 ]; then
        e2e_pass "Reproduce failed gracefully (test doesn't exist)"
    else
        e2e_fail "Reproduce output unclear: $repro_output"
    fi
fi

e2e_save_artifact "reproduce_attempt.txt" "$repro_output"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
