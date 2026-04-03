#!/usr/bin/env bash
# test_helpers.sh - Standardized transcript capture for E2E thin wrappers
#                   (br-aazao.8.4)
#
# Problem: Thin wrapper scripts in tests/e2e/ delegate to scripts/e2e_*.sh
# but don't themselves capture forensic artifacts. When a delegated suite
# fails, the wrapper-level context (environment, timing, exit code) is lost.
#
# Solution: Source this file in thin wrappers to get wrapper-level forensics
# that supplement the delegate suite's own artifacts.
#
# Usage (in a thin wrapper):
#   #!/usr/bin/env bash
#   WRAPPER_SUITE="archive"
#   SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
#   source "${SCRIPT_DIR}/test_helpers.sh"
#   wrapper_exec "${SCRIPT_DIR}/../../scripts/e2e_archive.sh"
#
# Artifacts captured by this helper:
#   tests/artifacts/<suite>/<timestamp>/wrapper/
#     env_at_start.txt      - Environment snapshot at wrapper entry
#     exit_code.txt         - Delegate script exit code
#     stderr_capture.txt    - Delegate script stderr (first 10000 lines)
#     timing.txt            - Wall-clock duration in seconds
#     wrapper_meta.json     - Machine-readable wrapper metadata
#
# This file does NOT replace e2e_lib.sh. It is a thin supplementary layer
# that ensures wrapper-level observability parity.

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

WRAPPER_SUITE="${WRAPPER_SUITE:-${E2E_SUITE:-unknown}}"

# Reuse e2e_lib.sh timestamp logic for consistent artifact paths
_WRAPPER_TIMESTAMP="${E2E_TIMESTAMP:-$(date -u '+%Y%m%d_%H%M%S')}"
_WRAPPER_PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
_WRAPPER_ARTIFACT_DIR="${_WRAPPER_PROJECT_ROOT}/tests/artifacts/${WRAPPER_SUITE}/${_WRAPPER_TIMESTAMP}"

# Safety: default to keeping temp dirs
: "${AM_E2E_KEEP_TMP:=1}"

# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

_wrapper_log() {
    echo -e "\033[0;90m[wrapper]\033[0m $*" >&2
}

_wrapper_dump_env() {
    echo "=== Wrapper Environment ==="
    echo "WRAPPER_SUITE=${WRAPPER_SUITE}"
    echo "timestamp=${_WRAPPER_TIMESTAMP}"
    echo "PWD=$(pwd)"
    echo "USER=${USER:-unknown}"
    echo "HOSTNAME=${HOSTNAME:-unknown}"
    echo "SHELL=${SHELL:-unknown}"
    echo "PATH=${PATH}"
    echo ""
    echo "=== E2E Variables ==="
    env | grep -E '^(E2E_|AM_E2E_|CARGO_TARGET|DATABASE_URL|STORAGE_ROOT|HTTP_|RUST_LOG|LOG_LEVEL)' \
        | sort || echo "(none set)"
    echo ""
    echo "=== System ==="
    uname -a 2>/dev/null || echo "(uname unavailable)"
    echo "date: $(date -u '+%Y-%m-%dT%H:%M:%SZ')"
}

_wrapper_save_artifact() {
    local name="$1"
    local content="$2"
    local dest="${_WRAPPER_ARTIFACT_DIR}/wrapper/${name}"
    mkdir -p "$(dirname "$dest")"
    echo "$content" > "$dest"
}

# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------

# wrapper_exec: Execute a delegate script with full forensic capture.
#
# This is the primary entry point for thin wrappers. It:
#   1. Creates the wrapper artifact directory
#   2. Captures the environment at entry
#   3. Runs the delegate, capturing stderr
#   4. Records exit code, timing, and metadata
#   5. Exits with the delegate's exit code
#
# Args:
#   $1: script_path - Path to the delegate script
#   $2+: Extra arguments passed to the delegate
#
# Example:
#   wrapper_exec "${SCRIPT_DIR}/../../scripts/e2e_archive.sh"
#   wrapper_exec "${SCRIPT_DIR}/../../scripts/e2e_cli.sh" "$@"

wrapper_exec() {
    local script_path="$1"
    shift

    mkdir -p "${_WRAPPER_ARTIFACT_DIR}/wrapper"

    # 1. Environment snapshot
    _wrapper_save_artifact "env_at_start.txt" "$(_wrapper_dump_env)"

    _wrapper_log "Suite: ${WRAPPER_SUITE}"
    _wrapper_log "Delegate: ${script_path}"
    _wrapper_log "Artifacts: ${_WRAPPER_ARTIFACT_DIR}/wrapper/"

    # 2. Run delegate with stderr capture
    local stderr_file="${_WRAPPER_ARTIFACT_DIR}/wrapper/stderr_capture.txt"
    local start_epoch
    start_epoch="$(date +%s)"

    set +e
    if [ $# -gt 0 ]; then
        bash "$script_path" "$@" 2> >(tee "$stderr_file" >&2)
    else
        bash "$script_path" 2> >(tee "$stderr_file" >&2)
    fi
    local exit_code=$?
    set -e

    local end_epoch
    end_epoch="$(date +%s)"
    local elapsed=$(( end_epoch - start_epoch ))

    # 3. Record exit code and timing
    _wrapper_save_artifact "exit_code.txt" "$exit_code"
    _wrapper_save_artifact "timing.txt" "${elapsed}s"

    # 4. Machine-readable metadata
    local started_at ended_at
    started_at="$(date -u -d "@${start_epoch}" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
        || date -u '+%Y-%m-%dT%H:%M:%SZ')"
    ended_at="$(date -u -d "@${end_epoch}" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null \
        || date -u '+%Y-%m-%dT%H:%M:%SZ')"

    _wrapper_save_artifact "wrapper_meta.json" "{
  \"schema_version\": 1,
  \"suite\": \"${WRAPPER_SUITE}\",
  \"delegate_script\": \"${script_path}\",
  \"timestamp\": \"${_WRAPPER_TIMESTAMP}\",
  \"started_at\": \"${started_at}\",
  \"ended_at\": \"${ended_at}\",
  \"elapsed_seconds\": ${elapsed},
  \"exit_code\": ${exit_code},
  \"wrapper_version\": \"br-aazao.8.4\"
}"

    if [ "$exit_code" -ne 0 ]; then
        _wrapper_log "Delegate exited with code ${exit_code} (artifacts in ${_WRAPPER_ARTIFACT_DIR}/wrapper/)"
    fi

    exit "$exit_code"
}

# wrapper_exec_passthrough: Like wrapper_exec but uses exec (replaces process).
#
# Use this for wrappers that use `exec` to delegate. The trade-off is that
# post-execution artifacts (exit_code, timing) won't be captured, but
# stderr and env will be.
#
# Args:
#   $1: script_path - Path to the delegate script
#   $2+: Extra arguments passed to the delegate

wrapper_exec_passthrough() {
    local script_path="$1"
    shift

    mkdir -p "${_WRAPPER_ARTIFACT_DIR}/wrapper"

    # Environment snapshot
    _wrapper_save_artifact "env_at_start.txt" "$(_wrapper_dump_env)"
    _wrapper_save_artifact "wrapper_meta.json" "{
  \"schema_version\": 1,
  \"suite\": \"${WRAPPER_SUITE}\",
  \"delegate_script\": \"${script_path}\",
  \"timestamp\": \"${_WRAPPER_TIMESTAMP}\",
  \"started_at\": \"$(date -u '+%Y-%m-%dT%H:%M:%SZ')\",
  \"mode\": \"passthrough\",
  \"wrapper_version\": \"br-aazao.8.4\"
}"

    _wrapper_log "Suite: ${WRAPPER_SUITE} (passthrough)"
    _wrapper_log "Delegate: ${script_path}"
    _wrapper_log "Artifacts: ${_WRAPPER_ARTIFACT_DIR}/wrapper/"

    exec bash "$script_path" "$@"
}
