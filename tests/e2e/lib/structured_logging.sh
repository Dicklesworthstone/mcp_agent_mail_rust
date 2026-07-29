#!/usr/bin/env bash
# Structured JSON-line logging helpers for E2E suites.

set -euo pipefail

structured_logging_help() {
    cat <<'EOF'
Usage:
  source tests/e2e/lib/structured_logging.sh
  log_event <level> <test> <scenario> <message> [<data_json>]
  log_pass <test> <scenario> <message> [<data_json>]
  log_fail <test> <scenario> <message> [<data_json>]
  log_warn <test> <scenario> <message> [<data_json>]
  log_info <test> <scenario> <message> [<data_json>]
  log_summary <test> <scenarios_total> <passed> <failed> <log_dir> <replay_command>

Emits one valid JSON object per line.
EOF
}

_am_e2e_json_string() {
    python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'
}

_am_e2e_data_json() {
    local data
    if [ "$#" -gt 0 ] && [ -n "${1}" ]; then
        data="$1"
    else
        data="{}"
    fi
    if python3 -c 'import json,sys; print(json.dumps(json.loads(sys.stdin.read()), separators=(",", ":")))' <<<"${data}" 2>/dev/null; then
        return 0
    else
        printf '{}'
    fi
}

log_event() {
    local level="$1"
    local test="$2"
    local scenario="$3"
    local message="$4"
    local data
    if [ "$#" -gt 4 ] && [ -n "${5}" ]; then
        data="$5"
    else
        data="{}"
    fi
    local ts
    ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

    printf '{"ts":"%s","test":%s,"scenario":%s,"level":%s,"message":%s,"data":%s}\n' \
        "${ts}" \
        "$(printf '%s' "${test}" | _am_e2e_json_string)" \
        "$(printf '%s' "${scenario}" | _am_e2e_json_string)" \
        "$(printf '%s' "${level}" | _am_e2e_json_string)" \
        "$(printf '%s' "${message}" | _am_e2e_json_string)" \
        "$(_am_e2e_data_json "${data}")"
}

log_pass() { log_event pass "$@"; }
log_fail() { log_event fail "$@"; }
log_warn() { log_event warn "$@"; }
log_info() { log_event info "$@"; }

log_summary() {
    local test="$1"
    local total="$2"
    local passed="$3"
    local failed="$4"
    local log_dir="$5"
    local replay="$6"
    local ts
    ts="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

    printf '{"ts":"%s","test":%s,"summary":{"scenarios":%d,"passed":%d,"failed":%d,"log_dir":%s,"replay_command":%s}}\n' \
        "${ts}" \
        "$(printf '%s' "${test}" | _am_e2e_json_string)" \
        "${total}" \
        "${passed}" \
        "${failed}" \
        "$(printf '%s' "${log_dir}" | _am_e2e_json_string)" \
        "$(printf '%s' "${replay}" | _am_e2e_json_string)"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    case "${1:-}" in
        -h|--help|"")
            structured_logging_help
            ;;
        *)
            structured_logging_help >&2
            exit 2
            ;;
    esac
fi
