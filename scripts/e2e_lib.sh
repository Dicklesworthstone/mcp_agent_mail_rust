#!/usr/bin/env bash
# e2e_lib.sh - Shared helpers for mcp-agent-mail E2E test suites
# Source this file from individual test scripts.
#
# Provides:
#   - Temp workspace creation + cleanup
#   - Artifact directory management
#   - Structured logging (banners, pass/fail, expected vs actual)
#   - File tree dumps and stable hashing
#   - Retry helpers for flaky port binds
#   - Environment dump (secrets redacted)

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

# Suite name: set by each test script before sourcing
E2E_SUITE="${E2E_SUITE:-unknown}"

# Keep temp dirs on failure for debugging
AM_E2E_KEEP_TMP="${AM_E2E_KEEP_TMP:-0}"

# Cargo target dir: avoid multi-agent contention
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="/data/tmp/cargo-target"
fi

# Root of the project
E2E_PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# Artifact directory for this run
E2E_TIMESTAMP="$(date -u '+%Y%m%d_%H%M%S')"
E2E_ARTIFACT_DIR="${E2E_PROJECT_ROOT}/tests/artifacts/${E2E_SUITE}/${E2E_TIMESTAMP}"

# Counters
_E2E_PASS=0
_E2E_FAIL=0
_E2E_SKIP=0
_E2E_TOTAL=0

# Temp dirs to clean up
_E2E_TMP_DIRS=()

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

_e2e_color_reset='\033[0m'
_e2e_color_green='\033[0;32m'
_e2e_color_red='\033[0;31m'
_e2e_color_yellow='\033[0;33m'
_e2e_color_blue='\033[0;34m'
_e2e_color_dim='\033[0;90m'

e2e_log() {
    echo -e "${_e2e_color_dim}[e2e]${_e2e_color_reset} $*"
}

e2e_banner() {
    local msg="$1"
    echo ""
    echo -e "${_e2e_color_blue}════════════════════════════════════════════════════════════${_e2e_color_reset}"
    echo -e "${_e2e_color_blue}  ${msg}${_e2e_color_reset}"
    echo -e "${_e2e_color_blue}════════════════════════════════════════════════════════════${_e2e_color_reset}"
}

e2e_case_banner() {
    local case_name="$1"
    (( _E2E_TOTAL++ )) || true
    echo ""
    echo -e "${_e2e_color_blue}── Case: ${case_name} ──${_e2e_color_reset}"
}

e2e_pass() {
    local msg="${1:-}"
    (( _E2E_PASS++ )) || true
    echo -e "  ${_e2e_color_green}PASS${_e2e_color_reset} ${msg}"
}

e2e_fail() {
    local msg="${1:-}"
    (( _E2E_FAIL++ )) || true
    echo -e "  ${_e2e_color_red}FAIL${_e2e_color_reset} ${msg}"
}

e2e_skip() {
    local msg="${1:-}"
    (( _E2E_SKIP++ )) || true
    echo -e "  ${_e2e_color_yellow}SKIP${_e2e_color_reset} ${msg}"
}

# Print expected vs actual for a mismatch
e2e_diff() {
    local label="$1"
    local expected="$2"
    local actual="$3"
    echo -e "  ${_e2e_color_red}MISMATCH${_e2e_color_reset} ${label}"
    echo -e "    expected: ${_e2e_color_green}${expected}${_e2e_color_reset}"
    echo -e "    actual:   ${_e2e_color_red}${actual}${_e2e_color_reset}"
}

# Assert two strings are equal
e2e_assert_eq() {
    local label="$1"
    local expected="$2"
    local actual="$3"
    if [ "$expected" = "$actual" ]; then
        e2e_pass "$label"
    else
        e2e_fail "$label"
        e2e_diff "$label" "$expected" "$actual"
    fi
}

# Assert a string contains a substring
e2e_assert_contains() {
    local label="$1"
    local haystack="$2"
    local needle="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        e2e_pass "$label"
    else
        e2e_fail "$label"
        echo -e "    expected to contain: ${_e2e_color_green}${needle}${_e2e_color_reset}"
        echo -e "    in: ${_e2e_color_red}${haystack}${_e2e_color_reset}"
    fi
}

# Assert a file exists
e2e_assert_file_exists() {
    local label="$1"
    local path="$2"
    if [ -f "$path" ]; then
        e2e_pass "$label"
    else
        e2e_fail "$label: file not found: $path"
    fi
}

# Assert a directory exists
e2e_assert_dir_exists() {
    local label="$1"
    local path="$2"
    if [ -d "$path" ]; then
        e2e_pass "$label"
    else
        e2e_fail "$label: directory not found: $path"
    fi
}

# Assert exit code
e2e_assert_exit_code() {
    local label="$1"
    local expected="$2"
    local actual="$3"
    if [ "$expected" = "$actual" ]; then
        e2e_pass "$label (exit=$actual)"
    else
        e2e_fail "$label"
        e2e_diff "exit code" "$expected" "$actual"
    fi
}

# ---------------------------------------------------------------------------
# Temp workspace management
# ---------------------------------------------------------------------------

# Create a temp directory and register it for cleanup
e2e_mktemp() {
    local prefix="${1:-e2e}"
    local td
    td="$(mktemp -d "/tmp/${prefix}.XXXXXX")"
    _E2E_TMP_DIRS+=("$td")
    echo "$td"
}

# Cleanup function: remove temp dirs unless AM_E2E_KEEP_TMP=1
_e2e_cleanup() {
    if [ "$AM_E2E_KEEP_TMP" = "1" ] || [ "$AM_E2E_KEEP_TMP" = "true" ]; then
        if [ ${#_E2E_TMP_DIRS[@]} -gt 0 ]; then
            e2e_log "Keeping temp dirs (AM_E2E_KEEP_TMP=1):"
            for d in "${_E2E_TMP_DIRS[@]}"; do
                e2e_log "  $d"
            done
        fi
        return
    fi
    for d in "${_E2E_TMP_DIRS[@]}"; do
        rm -rf "$d" 2>/dev/null || true
    done
}

trap _e2e_cleanup EXIT

# ---------------------------------------------------------------------------
# Artifact management
# ---------------------------------------------------------------------------

# Initialize the artifact directory for this run
e2e_init_artifacts() {
    mkdir -p "$E2E_ARTIFACT_DIR"
    e2e_log "Artifacts: $E2E_ARTIFACT_DIR"
}

# Save a file to the artifact directory
e2e_save_artifact() {
    local name="$1"
    local content="$2"
    local dest="${E2E_ARTIFACT_DIR}/${name}"
    mkdir -p "$(dirname "$dest")"
    echo "$content" > "$dest"
}

# Save a file (by path) to artifacts
e2e_copy_artifact() {
    local src="$1"
    local dest_name="${2:-$(basename "$src")}"
    local dest="${E2E_ARTIFACT_DIR}/${dest_name}"
    mkdir -p "$(dirname "$dest")"
    cp -r "$src" "$dest" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# File tree and hashing helpers
# ---------------------------------------------------------------------------

# Dump a directory tree (sorted, deterministic)
e2e_tree() {
    local dir="$1"
    find "$dir" -type f | sort | while read -r f; do
        local rel="${f#"$dir"/}"
        local sz
        sz=$(stat --format='%s' "$f" 2>/dev/null || stat -f '%z' "$f" 2>/dev/null || echo "?")
        echo "${rel} (${sz}b)"
    done
}

# Stable SHA256 of a file
e2e_sha256() {
    local file="$1"
    sha256sum "$file" 2>/dev/null | awk '{print $1}' || shasum -a 256 "$file" | awk '{print $1}'
}

# Stable SHA256 of a string
e2e_sha256_str() {
    local str="$1"
    echo -n "$str" | sha256sum 2>/dev/null | awk '{print $1}' || echo -n "$str" | shasum -a 256 | awk '{print $1}'
}

# ---------------------------------------------------------------------------
# Retry helper
# ---------------------------------------------------------------------------

# Retry a command with exponential backoff
# Usage: e2e_retry <max_attempts> <initial_delay_ms> <command...>
e2e_retry() {
    local max_attempts="$1"
    local delay_ms="$2"
    shift 2
    local attempt=1
    while [ $attempt -le "$max_attempts" ]; do
        if "$@"; then
            return 0
        fi
        if [ $attempt -eq "$max_attempts" ]; then
            return 1
        fi
        local delay_s
        delay_s=$(echo "scale=3; $delay_ms / 1000" | bc 2>/dev/null || echo "0.5")
        sleep "$delay_s"
        delay_ms=$(( delay_ms * 2 ))
        (( attempt++ )) || true
    done
    return 1
}

# Wait for a TCP port to become available
e2e_wait_port() {
    local host="${1:-127.0.0.1}"
    local port="$2"
    local timeout_s="${3:-10}"
    local deadline
    deadline=$(( $(date +%s) + timeout_s ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if bash -c "echo > /dev/tcp/${host}/${port}" 2>/dev/null; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# ---------------------------------------------------------------------------
# Environment dump (redact secrets)
# ---------------------------------------------------------------------------

e2e_dump_env() {
    e2e_log "Environment:"
    env | sort | while read -r line; do
        local key="${line%%=*}"
        local val="${line#*=}"
        # Redact anything that looks like a secret
        case "$key" in
            *SECRET*|*TOKEN*|*PASSWORD*|*KEY*|*CREDENTIAL*|*AUTH*)
                echo "  ${key}=<redacted>"
                ;;
            *)
                echo "  ${key}=${val}"
                ;;
        esac
    done
}

# ---------------------------------------------------------------------------
# Git helpers (safe, temp-dir only)
# ---------------------------------------------------------------------------

# Initialize a fresh git repo in a temp dir
e2e_init_git_repo() {
    local dir="$1"
    git -C "$dir" init -q
    git -C "$dir" config user.email "e2e@test.local"
    git -C "$dir" config user.name "E2E Test"
}

# Create a commit in a test repo
e2e_git_commit() {
    local dir="$1"
    local msg="${2:-test commit}"
    git -C "$dir" add -A
    git -C "$dir" commit -qm "$msg" --allow-empty
}

# ---------------------------------------------------------------------------
# Binary helpers
# ---------------------------------------------------------------------------

# Build the workspace binary (if needed)
e2e_ensure_binary() {
    local bin_name="${1:-mcp-agent-mail}"
    local bin_path="${CARGO_TARGET_DIR}/debug/${bin_name}"
    if [ ! -f "$bin_path" ] || [ "${E2E_FORCE_BUILD:-0}" = "1" ]; then
        e2e_log "Building ${bin_name}..."
        cargo build -p "$bin_name" 2>&1 | tail -5
    fi
    echo "$bin_path"
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_summary() {
    echo ""
    echo -e "${_e2e_color_blue}════════════════════════════════════════════════════════════${_e2e_color_reset}"
    echo -e "  Suite: ${E2E_SUITE}"
    echo -e "  Total: ${_E2E_TOTAL}  ${_e2e_color_green}Pass: ${_E2E_PASS}${_e2e_color_reset}  ${_e2e_color_red}Fail: ${_E2E_FAIL}${_e2e_color_reset}  ${_e2e_color_yellow}Skip: ${_E2E_SKIP}${_e2e_color_reset}"
    echo -e "  Artifacts: ${E2E_ARTIFACT_DIR}"
    echo -e "${_e2e_color_blue}════════════════════════════════════════════════════════════${_e2e_color_reset}"

    # Save summary to artifacts
    if [ -d "$E2E_ARTIFACT_DIR" ]; then
        cat > "${E2E_ARTIFACT_DIR}/summary.json" <<EOJSON
{
  "suite": "${E2E_SUITE}",
  "timestamp": "${E2E_TIMESTAMP}",
  "total": ${_E2E_TOTAL},
  "pass": ${_E2E_PASS},
  "fail": ${_E2E_FAIL},
  "skip": ${_E2E_SKIP}
}
EOJSON
    fi

    if [ "$_E2E_FAIL" -gt 0 ]; then
        return 1
    fi
    return 0
}
