#!/usr/bin/env bash
# ci.sh - Local CI runner: runs the same suite as .github/workflows/ci.yml
#
# Usage:
#   bash scripts/ci.sh          # Run all gates
#   bash scripts/ci.sh --quick  # Skip E2E (faster)
#
# Exit codes:
#   0 = all gates passed
#   1 = one or more gates failed

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$PROJECT_ROOT"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"
export DATABASE_URL="${DATABASE_URL:-sqlite:///tmp/ci_local.sqlite3}"
export STORAGE_ROOT="${STORAGE_ROOT:-/tmp/ci_storage}"
export AGENT_NAME="${AGENT_NAME:-CiLocalAgent}"
export HTTP_HOST="${HTTP_HOST:-127.0.0.1}"
export HTTP_PORT="${HTTP_PORT:-1}"
export HTTP_PATH="${HTTP_PATH:-/mcp/}"

QUICK=0
[ "${1:-}" = "--quick" ] && QUICK=1

PASS=0
FAIL=0
SKIP=0

gate() {
    local name="$1"
    shift
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  GATE: $name"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    local start_time
    start_time=$(date +%s)
    if "$@"; then
        local elapsed=$(( $(date +%s) - start_time ))
        echo "  PASS: $name (${elapsed}s)"
        PASS=$((PASS + 1))
    else
        local elapsed=$(( $(date +%s) - start_time ))
        echo "  FAIL: $name (${elapsed}s)"
        FAIL=$((FAIL + 1))
    fi
}

skip_gate() {
    local name="$1"
    echo "  SKIP: $name"
    SKIP=$((SKIP + 1))
}

# ── Gates ─────────────────────────────────────────────────────────────

gate "Format check" cargo fmt --all -- --check

gate "Clippy" cargo clippy --workspace --all-targets -- -D warnings

gate "Build workspace" cargo build --workspace

gate "Unit + integration tests" cargo test --workspace

gate "Mode matrix harness" cargo test -p mcp-agent-mail-cli --test mode_matrix_harness -- --nocapture

gate "Semantic conformance" cargo test -p mcp-agent-mail-cli --test semantic_conformance -- --nocapture

gate "Perf + security regressions" cargo test -p mcp-agent-mail-cli --test perf_security_regressions -- --nocapture

gate "Help snapshots" cargo test -p mcp-agent-mail-cli --test help_snapshots -- --nocapture

if [ "$QUICK" -eq 0 ]; then
    gate "E2E dual-mode" bash scripts/e2e_dual_mode.sh
    gate "E2E mode matrix" bash scripts/e2e_mode_matrix.sh
else
    skip_gate "E2E dual-mode (--quick)"
    skip_gate "E2E mode matrix (--quick)"
fi

# ── Summary ───────────────────────────────────────────────────────────

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  CI Summary: Pass=$PASS  Fail=$FAIL  Skip=$SKIP"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

if [ "$FAIL" -gt 0 ]; then
    echo "  RESULT: FAILED"
    exit 1
fi
echo "  RESULT: PASSED"
exit 0
