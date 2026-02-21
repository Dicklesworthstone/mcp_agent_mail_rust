#!/usr/bin/env bash
# e2e_test.sh - Compatibility shim for E2E test execution.
#
# Usage:
#   ./scripts/e2e_test.sh              # (Compat) delegates to: am e2e run --project <repo>
#   ./scripts/e2e_test.sh guard        # (Compat) delegates to: am e2e run --project <repo> guard
#   ./scripts/e2e_test.sh --list       # delegates to: am e2e list
#
# Environment:
#   AM_E2E_KEEP_TMP=1     Keep temp directories after run
#   E2E_FORCE_BUILD=1     Force rebuild before running
#   CARGO_TARGET_DIR=...  Override cargo target directory
#   AM_E2E_FORCE_LEGACY=1 Use legacy in-script runner (rollback path)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SUITES_DIR="${PROJECT_ROOT}/tests/e2e"

# Prefer a large temp root when available; keep target dir colocated with TMPDIR.
if [ -z "${TMPDIR:-}" ]; then
    if [ -d "/data/tmp" ]; then
        export TMPDIR="/data/tmp"
    else
        export TMPDIR="/tmp"
    fi
fi

# Set CARGO_TARGET_DIR if not already set (prevent multi-agent contention)
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="${TMPDIR%/}/cargo-target"
fi
mkdir -p "${CARGO_TARGET_DIR}" 2>/dev/null || true

# Colors
_c_reset='\033[0m'
_c_green='\033[0;32m'
_c_red='\033[0;31m'
_c_blue='\033[0;34m'
_c_yellow='\033[0;33m'

show_deprecation_notice() {
    cat >&2 <<'EOF'
[DEPRECATED] scripts/e2e_test.sh is now a compatibility shim.
Authoritative path: `am e2e run --project <repo> [suite...]`
Rollback path: set AM_E2E_FORCE_LEGACY=1 to use legacy in-script execution.
EOF
}

resolve_am_binary() {
    if [ -n "${AM_E2E_AM_BIN:-}" ] && [ -x "${AM_E2E_AM_BIN}" ]; then
        echo "${AM_E2E_AM_BIN}"
        return 0
    fi
    if [ -x "${CARGO_TARGET_DIR}/debug/am" ]; then
        echo "${CARGO_TARGET_DIR}/debug/am"
        return 0
    fi
    if [ -x "${PROJECT_ROOT}/target/debug/am" ]; then
        echo "${PROJECT_ROOT}/target/debug/am"
        return 0
    fi
    if command -v am >/dev/null 2>&1; then
        command -v am
        return 0
    fi
    return 1
}

maybe_delegate_native() {
    if [ "${AM_E2E_FORCE_LEGACY:-0}" = "1" ]; then
        return 0
    fi

    local am_bin
    if ! am_bin="$(resolve_am_binary)"; then
        return 0
    fi

    if [ "${AM_E2E_SILENCE_DEPRECATION:-0}" != "1" ]; then
        show_deprecation_notice
    fi

    local arg1="${1:-}"
    if [ "$arg1" = "--help" ] || [ "$arg1" = "-h" ]; then
        cat <<EOF
Usage: $0 [suite_name] [--list]

This script is deprecated as a primary entrypoint and now acts as a compatibility shim.

Primary (native) commands:
  ${am_bin} e2e list
  ${am_bin} e2e run --project ${PROJECT_ROOT} [suite_name]

Compatibility rollback:
  AM_E2E_FORCE_LEGACY=1 $0 [suite_name]
EOF
        exit 0
    fi

    if [ "$arg1" = "--list" ] || [ "$arg1" = "-l" ]; then
        (cd "${PROJECT_ROOT}" && "${am_bin}" e2e list)
        exit $?
    fi

    local cmd=("${am_bin}" "e2e" "run" "--project" "${PROJECT_ROOT}")
    if [ "${AM_E2E_KEEP_TMP:-0}" = "1" ]; then
        cmd+=("--keep-tmp")
    fi
    if [ "${E2E_FORCE_BUILD:-0}" = "1" ]; then
        cmd+=("--force-build")
    fi
    if [ -n "${E2E_TIMEOUT_SECS:-}" ]; then
        cmd+=("--timeout" "${E2E_TIMEOUT_SECS}")
    fi
    if [ -n "${AM_E2E_ARTIFACT_DIR:-}" ]; then
        cmd+=("--artifacts" "${AM_E2E_ARTIFACT_DIR}")
    fi
    if [ -n "$arg1" ]; then
        cmd+=("$arg1")
    fi

    echo -e "${_c_yellow}[compat-shim] delegating to native runner:${_c_reset} ${cmd[*]}" >&2
    (cd "${PROJECT_ROOT}" && "${cmd[@]}")
    exit $?
}

maybe_delegate_native "$@"

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
    echo "Run E2E test suites for mcp-agent-mail (compatibility mode)."
    echo ""
    echo "Primary native path (recommended):"
    echo "  am e2e run --project ${PROJECT_ROOT} [suite_name]"
    echo "  am e2e list"
    echo ""
    echo "Rollback (legacy in-script execution):"
    echo "  AM_E2E_FORCE_LEGACY=1 $0 [suite_name]"
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
echo -e "${_c_blue}║  mcp-agent-mail E2E Test Runner (Legacy Compat)        ║${_c_reset}"
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
    local cli_bin="${CARGO_TARGET_DIR}/debug/am"

    if [ ! -f "$suite_file" ]; then
        echo -e "${_c_red}Suite not found: ${suite_name}${_c_reset}"
        echo "  Expected: ${suite_file}"
        return 1
    fi

    (( total_suites++ )) || true
    echo -e "${_c_blue}Running suite: ${suite_name}${_c_reset}"
    echo "  Script: ${suite_file}"
    echo ""

    # Pilot migration: selected suites execute via native Rust runner.
    if [ "$suite_name" = "dual_mode" ] || [ "$suite_name" = "mode_matrix" ] || [ "$suite_name" = "security_privacy" ]; then
        if [ "${E2E_FORCE_BUILD:-0}" = "1" ] || [ ! -f "$cli_bin" ]; then
            echo "  Building native E2E runner binary (am)..."
            if ! cargo build -p mcp-agent-mail-cli > /dev/null 2>&1; then
                echo -e "${_c_red}Failed to build mcp-agent-mail-cli for native ${suite_name} run${_c_reset}"
                (( total_fail++ )) || true
                failed_suites+=("$suite_name")
                return
            fi
        fi

        local native_artifacts
        native_artifacts="${PROJECT_ROOT}/tests/artifacts_native"
        mkdir -p "$native_artifacts"
        echo "  Runner: native (am e2e)"
        echo "  Artifacts: ${native_artifacts}"

        if "$cli_bin" e2e run "$suite_name" --project "$PROJECT_ROOT" --artifacts "$native_artifacts"; then
            (( total_pass++ )) || true
        else
            (( total_fail++ )) || true
            failed_suites+=("$suite_name")
        fi
        return
    fi

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
