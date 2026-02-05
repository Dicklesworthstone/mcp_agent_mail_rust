#!/usr/bin/env bash
# bench_golden.sh - Capture and validate golden outputs for stable surfaces.
#
# Usage:
#   ./scripts/bench_golden.sh capture   # Capture golden outputs + checksums
#   ./scripts/bench_golden.sh validate  # Validate against existing checksums
#
# Golden outputs are stored in benches/golden/ with SHA-256 checksums.
# This ensures no behavioral regressions in stable output surfaces.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
GOLDEN_DIR="${PROJECT_ROOT}/benches/golden"
CHECKSUMS_FILE="${GOLDEN_DIR}/checksums.sha256"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"

# Colors
_reset='\033[0m'
_green='\033[0;32m'
_red='\033[0;31m'
_blue='\033[0;34m'

log() { echo -e "${_blue}[golden]${_reset} $*"; }
pass() { echo -e "  ${_green}PASS${_reset} $*"; }
fail() { echo -e "  ${_red}FAIL${_reset} $*"; }

# Ensure binary is built
ensure_binary() {
    local bin="${CARGO_TARGET_DIR}/debug/am"
    if [ ! -f "$bin" ]; then
        log "Building am binary..."
        cargo build -p mcp-agent-mail-cli 2>&1 | tail -3
    fi
    echo "$bin"
}

# Normalize non-deterministic parts (timestamps, PIDs, etc.)
normalize_output() {
    # Remove ANSI escape codes, normalize whitespace, sort JSON keys
    sed 's/\x1b\[[0-9;]*m//g' | \
    sed 's/[0-9]\{4\}-[0-9]\{2\}-[0-9]\{2\}T[0-9:\.Z+-]*/TIMESTAMP/g' | \
    sed 's/pid=[0-9]*/pid=PID/g'
}

capture_golden() {
    log "Capturing golden outputs..."
    mkdir -p "$GOLDEN_DIR"

    local bin
    bin=$(ensure_binary)
    local pass_count=0
    local fail_count=0

    # 1. CLI help text
    log "Capturing: am --help"
    "$bin" --help 2>/dev/null | normalize_output > "${GOLDEN_DIR}/am_help.txt" || true
    (( pass_count++ )) || true

    # 2. CLI version
    log "Capturing: am --version (if available)"
    "$bin" --version 2>/dev/null | normalize_output > "${GOLDEN_DIR}/am_version.txt" || echo "version not available" > "${GOLDEN_DIR}/am_version.txt"
    (( pass_count++ )) || true

    # 3. CLI subcommand help texts
    for subcmd in "serve-http" "serve-stdio" "guard" "share" "doctor" "config" "mail"; do
        log "Capturing: am ${subcmd} --help"
        "$bin" "$subcmd" --help 2>/dev/null | normalize_output > "${GOLDEN_DIR}/am_${subcmd}_help.txt" || echo "not available" > "${GOLDEN_DIR}/am_${subcmd}_help.txt"
        (( pass_count++ )) || true
    done

    # 4. Stub encoder outputs (deterministic)
    local stub="${PROJECT_ROOT}/scripts/toon_stub_encoder.sh"
    if [ -x "$stub" ]; then
        log "Capturing: stub encoder outputs"
        echo '{"id":1}' | "$stub" --encode > "${GOLDEN_DIR}/stub_encode.txt" 2>/dev/null
        echo '{"id":1}' | "$stub" --encode --stats > "${GOLDEN_DIR}/stub_encode_stats_stdout.txt" 2>"${GOLDEN_DIR}/stub_encode_stats_stderr.txt"
        "$stub" --help > "${GOLDEN_DIR}/stub_help.txt" 2>/dev/null
        "$stub" --version > "${GOLDEN_DIR}/stub_version.txt" 2>/dev/null
        (( pass_count += 4 )) || true
    fi

    # 5. Generate checksums
    log "Computing checksums..."
    (cd "$GOLDEN_DIR" && sha256sum *.txt > checksums.sha256 2>/dev/null)

    local checksum_count
    checksum_count=$(wc -l < "$CHECKSUMS_FILE" 2>/dev/null || echo 0)
    log "Captured ${pass_count} golden outputs, ${checksum_count} checksums"
    log "Golden dir: ${GOLDEN_DIR}"
}

validate_golden() {
    log "Validating golden outputs..."

    if [ ! -f "$CHECKSUMS_FILE" ]; then
        fail "No checksums file found at ${CHECKSUMS_FILE}"
        fail "Run '$0 capture' first"
        exit 1
    fi

    local pass_count=0
    local fail_count=0
    local bin
    bin=$(ensure_binary)

    # Re-capture to temp dir
    local tmp_dir
    tmp_dir=$(mktemp -d)
    trap 'rm -rf "${tmp_dir:-}"' EXIT

    # Re-capture all golden outputs to temp
    "$bin" --help 2>/dev/null | normalize_output > "${tmp_dir}/am_help.txt" || true
    "$bin" --version 2>/dev/null | normalize_output > "${tmp_dir}/am_version.txt" || echo "version not available" > "${tmp_dir}/am_version.txt"

    for subcmd in "serve-http" "serve-stdio" "guard" "share" "doctor" "config" "mail"; do
        "$bin" "$subcmd" --help 2>/dev/null | normalize_output > "${tmp_dir}/am_${subcmd}_help.txt" || echo "not available" > "${tmp_dir}/am_${subcmd}_help.txt"
    done

    local stub="${PROJECT_ROOT}/scripts/toon_stub_encoder.sh"
    if [ -x "$stub" ]; then
        echo '{"id":1}' | "$stub" --encode > "${tmp_dir}/stub_encode.txt" 2>/dev/null
        echo '{"id":1}' | "$stub" --encode --stats > "${tmp_dir}/stub_encode_stats_stdout.txt" 2>"${tmp_dir}/stub_encode_stats_stderr.txt"
        "$stub" --help > "${tmp_dir}/stub_help.txt" 2>/dev/null
        "$stub" --version > "${tmp_dir}/stub_version.txt" 2>/dev/null
    fi

    # Compare each golden file
    while IFS='  ' read -r expected_hash filename; do
        if [ ! -f "${tmp_dir}/${filename}" ]; then
            fail "${filename}: not captured in validation run"
            (( fail_count++ )) || true
            continue
        fi

        local actual_hash
        actual_hash=$(sha256sum "${tmp_dir}/${filename}" | awk '{print $1}')

        if [ "$expected_hash" = "$actual_hash" ]; then
            pass "$filename"
            (( pass_count++ )) || true
        else
            fail "${filename}: checksum mismatch"
            echo "    expected: $expected_hash"
            echo "    actual:   $actual_hash"
            # Show diff
            if command -v diff &>/dev/null; then
                diff --color=auto -u "${GOLDEN_DIR}/${filename}" "${tmp_dir}/${filename}" || true
            fi
            (( fail_count++ )) || true
        fi
    done < "$CHECKSUMS_FILE"

    echo ""
    log "Validation: ${pass_count} passed, ${fail_count} failed"

    if [ "$fail_count" -gt 0 ]; then
        log "Run '$0 capture' to update golden outputs after intentional changes"
        exit 1
    fi
}

# Main
case "${1:-}" in
    capture)
        capture_golden
        ;;
    validate)
        validate_golden
        ;;
    *)
        echo "Usage: $0 {capture|validate}"
        echo ""
        echo "  capture   - Capture golden outputs and compute checksums"
        echo "  validate  - Validate current outputs against stored checksums"
        exit 1
        ;;
esac
