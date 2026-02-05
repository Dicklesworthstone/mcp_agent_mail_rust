#!/usr/bin/env bash
# e2e_test.sh - Top-level E2E test runner for mcp-agent-mail Rust port
#
# Usage:
#   ./scripts/e2e_test.sh              # Run all suites
#   ./scripts/e2e_test.sh guard        # Run a specific suite
#   ./scripts/e2e_test.sh --list       # List available suites
#
# Environment:
#   AM_E2E_KEEP_TMP=1     Keep temp directories after run
#   E2E_FORCE_BUILD=1     Force rebuild before running
#   CARGO_TARGET_DIR=...  Override cargo target directory

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SUITES_DIR="${PROJECT_ROOT}/tests/e2e"

# Set CARGO_TARGET_DIR if not already set (prevent multi-agent contention)
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="/data/tmp/cargo-target"
fi

# Colors
_c_reset='\033[0m'
_c_green='\033[0;32m'
_c_red='\033[0;31m'
_c_blue='\033[0;34m'

# ---------------------------------------------------------------------------
# Suite discovery
# ---------------------------------------------------------------------------

list_suites() {
    local suites=()
    for f in "${SUITES_DIR}"/test_*.sh; do
        [ -f "$f" ] || continue
        local name
        name="$(basename "$f")"
        name="${name#test_}"
        name="${name%.sh}"
        suites+=("$name")
    done
    echo "${suites[@]}"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

if [ "${1:-}" = "--list" ] || [ "${1:-}" = "-l" ]; then
    echo "Available E2E test suites:"
    for s in $(list_suites); do
        echo "  $s"
    done
    exit 0
fi

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    echo "Usage: $0 [suite_name] [--list]"
    echo ""
    echo "Run E2E test suites for mcp-agent-mail."
    echo ""
    echo "Options:"
    echo "  --list, -l    List available suites"
    echo "  --help, -h    Show this help"
    echo ""
    echo "Environment:"
    echo "  AM_E2E_KEEP_TMP=1     Keep temp directories"
    echo "  E2E_FORCE_BUILD=1     Force rebuild"
    echo "  CARGO_TARGET_DIR=...  Override cargo target"
    exit 0
fi

echo ""
echo -e "${_c_blue}╔══════════════════════════════════════════════════════════╗${_c_reset}"
echo -e "${_c_blue}║  mcp-agent-mail E2E Test Runner                        ║${_c_reset}"
echo -e "${_c_blue}╚══════════════════════════════════════════════════════════╝${_c_reset}"
echo ""
echo "  Project:     ${PROJECT_ROOT}"
echo "  Target dir:  ${CARGO_TARGET_DIR}"
echo "  Keep tmp:    ${AM_E2E_KEEP_TMP:-0}"
echo ""

# Determine which suites to run
TARGET_SUITE="${1:-}"
total_pass=0
total_fail=0
total_suites=0
failed_suites=()

run_suite() {
    local suite_name="$1"
    local suite_file="${SUITES_DIR}/test_${suite_name}.sh"

    if [ ! -f "$suite_file" ]; then
        echo -e "${_c_red}Suite not found: ${suite_name}${_c_reset}"
        echo "  Expected: ${suite_file}"
        return 1
    fi

    (( total_suites++ )) || true
    echo -e "${_c_blue}Running suite: ${suite_name}${_c_reset}"
    echo "  Script: ${suite_file}"
    echo ""

    if bash "$suite_file"; then
        (( total_pass++ )) || true
    else
        (( total_fail++ )) || true
        failed_suites+=("$suite_name")
    fi
}

if [ -n "$TARGET_SUITE" ]; then
    run_suite "$TARGET_SUITE"
else
    suites=($(list_suites))
    if [ ${#suites[@]} -eq 0 ]; then
        echo "No E2E test suites found in ${SUITES_DIR}/"
        echo "Create test scripts as tests/e2e/test_<name>.sh"
        exit 0
    fi
    for s in "${suites[@]}"; do
        run_suite "$s"
    done
fi

# Summary
echo ""
echo -e "${_c_blue}╔══════════════════════════════════════════════════════════╗${_c_reset}"
echo -e "${_c_blue}║  E2E Summary                                           ║${_c_reset}"
echo -e "${_c_blue}╚══════════════════════════════════════════════════════════╝${_c_reset}"
echo ""
echo -e "  Suites run: ${total_suites}"
echo -e "  ${_c_green}Passed: ${total_pass}${_c_reset}"
echo -e "  ${_c_red}Failed: ${total_fail}${_c_reset}"

if [ ${#failed_suites[@]} -gt 0 ]; then
    echo ""
    echo -e "  ${_c_red}Failed suites:${_c_reset}"
    for s in "${failed_suites[@]}"; do
        echo -e "    - ${s}"
    done
fi

echo ""

if [ "$total_fail" -gt 0 ]; then
    exit 1
fi
exit 0
