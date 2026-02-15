#!/usr/bin/env bash
# t16_validate.sh - Deterministic Test Orchestration for T16 Showcase Parity (br-1xt0m.1.13.16)
#
# Single entrypoint to validate the full T16 (TUI Showcase Parity) scope.
# Runs unit → snapshot → E2E → perf tests in dependency order with fail-fast
# semantics and structured diagnostics output.
#
# Usage:
#   ./scripts/t16_validate.sh                  # Run full T16 validation
#   ./scripts/t16_validate.sh --unit-only      # Run only Rust unit tests
#   ./scripts/t16_validate.sh --e2e-only       # Run only E2E shell tests
#   ./scripts/t16_validate.sh --perf-only      # Run only perf regression tests
#   ./scripts/t16_validate.sh --no-fail-fast   # Continue past failures
#   ./scripts/t16_validate.sh --list           # List all phases and suites
#   ./scripts/t16_validate.sh --dry-run        # Show what would run without executing
#
# Environment:
#   T16_FAIL_FAST=0             Disable fail-fast (default: 1)
#   SOAK_DURATION_SECS=10       Soak test duration (default: 10)
#   CARGO_TARGET_DIR=...        Override cargo target directory
#   T16_SKIP_BUILD=1            Skip compilation check phase
#   T16_SKIP_PERF=1             Skip performance regression phase

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SUITES_DIR="${PROJECT_ROOT}/tests/e2e"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"
export CARGO_TARGET_DIR

FAIL_FAST="${T16_FAIL_FAST:-1}"
SOAK_DURATION_SECS="${SOAK_DURATION_SECS:-10}"
export SOAK_DURATION_SECS

# Parse CLI flags
UNIT_ONLY=0
E2E_ONLY=0
PERF_ONLY=0
DRY_RUN=0
for arg in "$@"; do
    case "$arg" in
        --unit-only)  UNIT_ONLY=1 ;;
        --e2e-only)   E2E_ONLY=1 ;;
        --perf-only)  PERF_ONLY=1 ;;
        --no-fail-fast) FAIL_FAST=0 ;;
        --dry-run)    DRY_RUN=1 ;;
        --list)
            echo "T16 Validation Phases:"
            echo ""
            echo "Phase 1 — Build Verification"
            echo "  cargo check -p mcp-agent-mail-server"
            echo ""
            echo "Phase 2 — Rust Unit Tests"
            echo "  cargo test -p mcp-agent-mail-server --lib"
            echo ""
            echo "Phase 3 — Rust Snapshot/Integration Tests"
            echo "  cargo test -p mcp-agent-mail-server --test golden_snapshots"
            echo "  cargo test -p mcp-agent-mail-server --test golden_markdown_snapshots"
            echo "  cargo test -p mcp-agent-mail-server --test pty_e2e_search"
            echo ""
            echo "Phase 4 — E2E Shell Suites (dependency order)"
            echo "  test_logging_contract.sh       (br-1xt0m.1.13.13)"
            echo "  test_failure_injection.sh       (br-1xt0m.1.13.15)"
            echo "  test_stdio_workflows.sh         (br-1xt0m.1.13.10)"
            echo "  test_stdio_screen_workflows.sh  (br-1xt0m.1.13.11)"
            echo "  test_stdio_adaptive.sh          (br-1xt0m.1.13.12)"
            echo "  test_artifacts_schema.sh        (harness validation)"
            echo ""
            echo "Phase 5 — Performance Regression"
            echo "  test_perf_regression.sh         (br-1xt0m.1.13.14)"
            echo ""
            echo "Phase 6 — Evidence Summary"
            echo "  Aggregate pass/fail/skip counts and artifact locations"
            exit 0
            ;;
        --help|-h)
            head -20 "$0" | tail -15
            exit 0
            ;;
    esac
done

# ── Colors ───────────────────────────────────────────────────────────

_c_reset='\033[0m'
_c_green='\033[0;32m'
_c_red='\033[0;31m'
_c_yellow='\033[0;33m'
_c_blue='\033[0;34m'
_c_bold='\033[1m'

# ── State ────────────────────────────────────────────────────────────

T16_START_EPOCH="$(date +%s)"
T16_ARTIFACT_DIR="${PROJECT_ROOT}/tests/artifacts/t16_validate/$(date +%Y%m%d_%H%M%S)_$$"
mkdir -p "${T16_ARTIFACT_DIR}/phases" "${T16_ARTIFACT_DIR}/diagnostics"

declare -a PHASE_NAMES=()
declare -a PHASE_STATUSES=()
declare -a PHASE_DURATIONS=()
declare -a SUITE_LOG=()
TOTAL_PHASES=0
PASSED_PHASES=0
FAILED_PHASES=0
SKIPPED_PHASES=0
ABORT=0

# ── Helpers ──────────────────────────────────────────────────────────

phase_banner() {
    local phase_num="$1" phase_name="$2"
    echo ""
    echo -e "${_c_blue}${_c_bold}━━━ Phase ${phase_num}: ${phase_name} ━━━${_c_reset}"
    echo ""
}

record_phase() {
    local name="$1" status="$2" duration="$3"
    PHASE_NAMES+=("$name")
    PHASE_STATUSES+=("$status")
    PHASE_DURATIONS+=("$duration")
    (( TOTAL_PHASES++ )) || true
    case "$status" in
        PASS) (( PASSED_PHASES++ )) || true ;;
        FAIL) (( FAILED_PHASES++ )) || true ;;
        SKIP) (( SKIPPED_PHASES++ )) || true ;;
    esac
}

check_abort() {
    if [ "$FAIL_FAST" = "1" ] && [ "$ABORT" = "1" ]; then
        echo -e "${_c_red}Fail-fast: aborting remaining phases${_c_reset}"
        return 1
    fi
    return 0
}

run_cargo_cmd() {
    local label="$1"
    shift
    local log_file="${T16_ARTIFACT_DIR}/phases/${label//\//_}.log"
    local start_s
    start_s="$(date +%s)"

    if [ "$DRY_RUN" = "1" ]; then
        echo "  [dry-run] cargo $*"
        record_phase "$label" "SKIP" "0"
        return 0
    fi

    echo -e "  Running: cargo $*"
    set +e
    cargo "$@" >"$log_file" 2>&1
    local rc=$?
    set -e
    local duration=$(( $(date +%s) - start_s ))

    if [ "$rc" -eq 0 ]; then
        echo -e "  ${_c_green}PASS${_c_reset} ${label} (${duration}s)"
        record_phase "$label" "PASS" "$duration"
    else
        echo -e "  ${_c_red}FAIL${_c_reset} ${label} (${duration}s)"
        echo "  Log: ${log_file}"
        { tail -20 "$log_file" 2>/dev/null || true; } | sed 's/^/    /'
        record_phase "$label" "FAIL" "$duration"
        ABORT=1
    fi
}

run_e2e_suite() {
    local suite_name="$1"
    local suite_file="${SUITES_DIR}/test_${suite_name}.sh"
    local log_file="${T16_ARTIFACT_DIR}/phases/e2e_${suite_name}.log"
    local start_s
    start_s="$(date +%s)"

    if [ ! -f "$suite_file" ]; then
        echo -e "  ${_c_yellow}SKIP${_c_reset} ${suite_name} (not found: ${suite_file})"
        record_phase "e2e/${suite_name}" "SKIP" "0"
        return 0
    fi

    if [ "$DRY_RUN" = "1" ]; then
        echo "  [dry-run] bash ${suite_file}"
        record_phase "e2e/${suite_name}" "SKIP" "0"
        return 0
    fi

    echo -e "  Running: ${suite_name}"
    set +e
    bash "$suite_file" >"$log_file" 2>&1
    local rc=$?
    set -e
    local duration=$(( $(date +%s) - start_s ))

    # Extract pass/fail counts from the suite's own summary (pipefail-safe).
    local suite_summary=""
    suite_summary="$(tail -10 "$log_file" 2>/dev/null | { grep -a 'Pass:' || true; })" || true

    if [ "$rc" -eq 0 ]; then
        echo -e "  ${_c_green}PASS${_c_reset} ${suite_name} (${duration}s) ${suite_summary}"
        record_phase "e2e/${suite_name}" "PASS" "$duration"
    else
        echo -e "  ${_c_red}FAIL${_c_reset} ${suite_name} (${duration}s) ${suite_summary}"
        echo "  Log: ${log_file}"
        { tail -15 "$log_file" 2>/dev/null || true; } | sed 's/^/    /'
        record_phase "e2e/${suite_name}" "FAIL" "$duration"
        ABORT=1
    fi
}

# ── Banner ───────────────────────────────────────────────────────────

echo ""
echo -e "${_c_blue}${_c_bold}╔══════════════════════════════════════════════════════════════╗${_c_reset}"
echo -e "${_c_blue}${_c_bold}║  T16 Showcase Parity — Deterministic Validation Suite       ║${_c_reset}"
echo -e "${_c_blue}${_c_bold}╚══════════════════════════════════════════════════════════════╝${_c_reset}"
echo ""
echo "  Project:     ${PROJECT_ROOT}"
echo "  Target dir:  ${CARGO_TARGET_DIR}"
echo "  Fail-fast:   ${FAIL_FAST}"
echo "  Soak secs:   ${SOAK_DURATION_SECS}"
echo "  Artifacts:   ${T16_ARTIFACT_DIR}"
echo "  Dry run:     ${DRY_RUN}"
if [ "$UNIT_ONLY" = "1" ]; then echo "  Scope:       unit-only"; fi
if [ "$E2E_ONLY" = "1" ]; then echo "  Scope:       e2e-only"; fi
if [ "$PERF_ONLY" = "1" ]; then echo "  Scope:       perf-only"; fi

# ── Phase 1: Build Verification ─────────────────────────────────────

if [ "$E2E_ONLY" = "0" ] && [ "$PERF_ONLY" = "0" ] && [ "${T16_SKIP_BUILD:-0}" != "1" ]; then
    phase_banner 1 "Build Verification"

    set +e
    run_cargo_cmd "build/check" check -p mcp-agent-mail-server
    build_rc=$?
    set -e

    if [ "$build_rc" -ne 0 ]; then
        echo -e "${_c_red}Build failed; cannot proceed with further phases.${_c_reset}"
        # Still generate evidence summary
    fi
    check_abort || true
fi

# ── Phase 2: Rust Unit Tests ────────────────────────────────────────

if [ "$E2E_ONLY" = "0" ] && [ "$PERF_ONLY" = "0" ] && [ "$ABORT" = "0" ]; then
    phase_banner 2 "Rust Unit Tests"

    set +e
    run_cargo_cmd "unit/server-lib" test -p mcp-agent-mail-server --lib
    set -e
    check_abort || true
fi

# ── Phase 3: Rust Snapshot/Integration Tests ────────────────────────

if [ "$E2E_ONLY" = "0" ] && [ "$PERF_ONLY" = "0" ] && [ "$ABORT" = "0" ]; then
    phase_banner 3 "Snapshot and Integration Tests"

    for test_name in golden_snapshots golden_markdown_snapshots pty_e2e_search; do
        if [ "$ABORT" = "1" ] && [ "$FAIL_FAST" = "1" ]; then break; fi
        set +e
        run_cargo_cmd "snapshot/${test_name}" test -p mcp-agent-mail-server --test "$test_name"
        set -e
    done
    check_abort || true
fi

# ── Phase 4: E2E Shell Suites ───────────────────────────────────────

if [ "$UNIT_ONLY" = "0" ] && [ "$PERF_ONLY" = "0" ] && [ "$ABORT" = "0" ]; then
    phase_banner 4 "E2E Shell Suites"

    # Ordered by dependency chain (earlier beads unblock later ones).
    T16_E2E_SUITES=(
        logging_contract          # br-1xt0m.1.13.13 — harness contract
        failure_injection         # br-1xt0m.1.13.15 — error/degraded paths
        stdio_workflows           # br-1xt0m.1.13.10 — shell navigation + MCP
        stdio_screen_workflows    # br-1xt0m.1.13.11 — screen operator workflows
        stdio_adaptive            # br-1xt0m.1.13.12 — responsive matrix
        artifacts_schema          # harness validation (bundle manifest)
    )

    for suite in "${T16_E2E_SUITES[@]}"; do
        if [ "$ABORT" = "1" ] && [ "$FAIL_FAST" = "1" ]; then break; fi
        set +e
        run_e2e_suite "$suite"
        set -e
    done
    check_abort || true
fi

# ── Phase 5: Performance Regression ─────────────────────────────────

if [ "$UNIT_ONLY" = "0" ] && [ "$ABORT" = "0" ] && [ "${T16_SKIP_PERF:-0}" != "1" ]; then
    phase_banner 5 "Performance Regression"

    set +e
    run_e2e_suite "perf_regression"
    set -e
    check_abort || true
fi

# ── Phase 6: Evidence Summary ────────────────────────────────────────

T16_END_EPOCH="$(date +%s)"
T16_DURATION=$(( T16_END_EPOCH - T16_START_EPOCH ))

echo ""
echo -e "${_c_blue}${_c_bold}╔══════════════════════════════════════════════════════════════╗${_c_reset}"
echo -e "${_c_blue}${_c_bold}║  T16 Validation Evidence Summary                            ║${_c_reset}"
echo -e "${_c_blue}${_c_bold}╚══════════════════════════════════════════════════════════════╝${_c_reset}"
echo ""

# Phase table
printf "  %-40s %-6s %6s\n" "Phase" "Status" "Time"
printf "  %s\n" "────────────────────────────────────────────────────────"
for i in "${!PHASE_NAMES[@]}"; do
    local_status="${PHASE_STATUSES[$i]}"
    case "$local_status" in
        PASS) color="${_c_green}" ;;
        FAIL) color="${_c_red}" ;;
        SKIP) color="${_c_yellow}" ;;
        *)    color="${_c_reset}" ;;
    esac
    printf "  %-40s ${color}%-6s${_c_reset} %5ss\n" \
        "${PHASE_NAMES[$i]}" "$local_status" "${PHASE_DURATIONS[$i]}"
done

echo ""
echo -e "  Total phases:  ${TOTAL_PHASES}"
echo -e "  ${_c_green}Passed:  ${PASSED_PHASES}${_c_reset}"
echo -e "  ${_c_red}Failed:  ${FAILED_PHASES}${_c_reset}"
echo -e "  ${_c_yellow}Skipped: ${SKIPPED_PHASES}${_c_reset}"
echo -e "  Duration:      ${T16_DURATION}s"
echo -e "  Artifacts:     ${T16_ARTIFACT_DIR}"
echo ""

# Write JSON evidence
python3 - "${T16_ARTIFACT_DIR}/evidence.json" "$T16_DURATION" "$TOTAL_PHASES" \
    "$PASSED_PHASES" "$FAILED_PHASES" "$SKIPPED_PHASES" <<'PY'
import json, sys, os, platform, subprocess

dest = sys.argv[1]
duration = int(sys.argv[2])
total = int(sys.argv[3])
passed = int(sys.argv[4])
failed = int(sys.argv[5])
skipped = int(sys.argv[6])

def cmd(args):
    try:
        return subprocess.check_output(args, stderr=subprocess.DEVNULL, timeout=5).decode().strip()
    except Exception:
        return "unknown"

evidence = {
    "bead": "br-1xt0m.1.13.16",
    "suite": "t16_validate",
    "verdict": "PASS" if failed == 0 else "FAIL",
    "phases": {"total": total, "passed": passed, "failed": failed, "skipped": skipped},
    "duration_s": duration,
    "environment": {
        "hostname": platform.node(),
        "arch": platform.machine(),
        "cpus": os.cpu_count(),
        "rustc": cmd(["rustc", "--version"]),
        "git_sha": cmd(["git", "rev-parse", "--short", "HEAD"]),
    },
}

with open(dest, "w") as f:
    json.dump(evidence, f, indent=2)
    f.write("\n")
PY

if [ "$FAILED_PHASES" -gt 0 ]; then
    echo -e "${_c_red}${_c_bold}T16 VALIDATION FAILED${_c_reset}"
    echo ""
    exit 1
fi

echo -e "${_c_green}${_c_bold}T16 VALIDATION PASSED${_c_reset}"
echo ""
exit 0
