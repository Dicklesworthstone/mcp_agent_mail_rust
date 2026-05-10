#!/usr/bin/env bash
# Generic wait-for-condition helper for E2E scripts.

set -euo pipefail

wait_for_help() {
    cat <<'EOF'
Usage:
  source tests/e2e/lib/wait_for.sh
  wait_for_condition <timeout_seconds> <interval_seconds> -- <command> [args...]

Returns 0 when the command succeeds before timeout, otherwise returns 124.
EOF
}

wait_for_condition() {
    local timeout_s="$1"
    local interval_s="$2"
    shift 2
    if [ "${1:-}" = "--" ]; then
        shift
    fi
    if [ "$#" -eq 0 ]; then
        echo "wait_for_condition: missing command" >&2
        return 2
    fi

    local start now elapsed
    start="$(date +%s)"
    while true; do
        if "$@" >/dev/null 2>&1; then
            return 0
        fi
        now="$(date +%s)"
        elapsed=$((now - start))
        if [ "${elapsed}" -ge "${timeout_s}" ]; then
            return 124
        fi
        sleep "${interval_s}"
    done
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "${1:-}" in
        -h|--help|"")
            wait_for_help
            ;;
        *)
            wait_for_help >&2
            exit 2
            ;;
    esac
fi
