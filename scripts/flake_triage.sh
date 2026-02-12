#!/usr/bin/env bash
# ═══════════════════════════════════════════════════════════════════════════
# DEPRECATED: Use `am flake-triage` instead.
#
# Native Rust implementation with no python3 dependency.
# Equivalent commands:
#   am flake-triage scan [--dir DIR] [--json]     # Scan for artifacts
#   am flake-triage reproduce <artifact.json>     # Reproduce from artifact
#   am flake-triage detect <test> [--seeds N]     # Multi-seed flake detection
#
# This script is maintained for backward compatibility only.
# ═══════════════════════════════════════════════════════════════════════════
#
# br-3vwi.10.5: Flake triage CLI — analyze and reproduce test failures.
#
# Usage:
#   scripts/flake_triage.sh <failure_context.json>    Reproduce from artifact
#   scripts/flake_triage.sh --scan [dir]              Scan for recent failures
#   scripts/flake_triage.sh --multi-seed <test> [N]   Run test with multiple seeds
#
# Examples:
#   scripts/flake_triage.sh tests/artifacts/flake_triage/20260210_*/failure_context.json
#   scripts/flake_triage.sh --scan tests/artifacts/
#   scripts/flake_triage.sh --multi-seed my_test_name 20

set -euo pipefail

# ── Deprecation Warning ──────────────────────────────────────────────────
echo -e "\033[0;33m[DEPRECATED]\033[0m scripts/flake_triage.sh is deprecated. Use 'am flake-triage' instead." >&2

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# ── Colors ────────────────────────────────────────────────────────────

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
CYAN='\033[0;36m'
BOLD='\033[1m'
NC='\033[0m'

# ── Helpers ───────────────────────────────────────────────────────────

info()  { echo -e "${CYAN}[info]${NC} $*"; }
warn()  { echo -e "${YELLOW}[warn]${NC} $*"; }
error() { echo -e "${RED}[error]${NC} $*" >&2; }
ok()    { echo -e "${GREEN}[ok]${NC} $*"; }

usage() {
    cat <<'USAGE'
Flake Triage CLI — Analyze and reproduce test failures

COMMANDS:
  <failure_context.json>        Reproduce a failure from its artifact
  --scan [dir]                  Scan for recent failure artifacts
  --multi-seed <test> [N]       Run a test with N seeds (default: 17)
  --help                        Show this help

EXAMPLES:
  # Reproduce a specific failure:
  scripts/flake_triage.sh tests/artifacts/flake_triage/20260210_*/failure_context.json

  # Scan for all recent failures:
  scripts/flake_triage.sh --scan

  # Run flake detection with 20 seeds:
  scripts/flake_triage.sh --multi-seed "my_test_name" 20
USAGE
    exit 0
}

# ── Reproduce from Artifact ───────────────────────────────────────────

reproduce_from_artifact() {
    local artifact="$1"

    if [[ ! -f "$artifact" ]]; then
        error "Artifact not found: $artifact"
        exit 1
    fi

    info "Reading failure context: $artifact"

    local test_name seed repro_cmd category
    test_name="$(python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(d['test_name'])" "$artifact" 2>/dev/null)"
    seed="$(python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(d.get('harness_seed',''))" "$artifact" 2>/dev/null)"
    repro_cmd="$(python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(d['repro_command'])" "$artifact" 2>/dev/null)"
    category="$(python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(d['category'])" "$artifact" 2>/dev/null)"

    echo ""
    echo -e "${BOLD}Test:${NC}     $test_name"
    echo -e "${BOLD}Seed:${NC}     ${seed:-N/A}"
    echo -e "${BOLD}Category:${NC} $category"
    echo -e "${BOLD}Repro:${NC}    $repro_cmd"
    echo ""

    read -rp "Run reproduction? [y/N] " confirm
    if [[ "$confirm" =~ ^[Yy] ]]; then
        info "Running: $repro_cmd"
        eval "$repro_cmd" || true
    fi
}

# ── Scan for Failures ─────────────────────────────────────────────────

scan_failures() {
    local scan_dir="${1:-$REPO_ROOT/tests/artifacts}"

    info "Scanning for failure artifacts in: $scan_dir"

    local count=0
    while IFS= read -r -d '' artifact; do
        count=$((count + 1))
        local test_name category failure_ts
        test_name="$(python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(d['test_name'])" "$artifact" 2>/dev/null)"
        category="$(python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(d['category'])" "$artifact" 2>/dev/null)"
        failure_ts="$(python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(d['failure_ts'][:19])" "$artifact" 2>/dev/null)"

        echo -e "  ${BOLD}[$count]${NC} ${failure_ts} ${RED}${test_name}${NC} (${category})"
        echo "      $artifact"
    done < <(find "$scan_dir" -name "failure_context.json" -print0 2>/dev/null | sort -z)

    if [[ $count -eq 0 ]]; then
        ok "No failure artifacts found."
    else
        warn "Found $count failure artifact(s)."
    fi
}

# ── Multi-Seed Run ────────────────────────────────────────────────────

multi_seed_run() {
    local test_name="$1"
    local num_seeds="${2:-17}"

    info "Running $test_name with $num_seeds seeds..."

    # Default seed corpus
    local seeds=(0 1 2 42 100 255 1000 12345 65535 999999
                 3735928559 3405691582 305419896 4294967295
                 18446744073709551615 9223372036854775807 6148914691236517205)

    # Extend with random seeds if needed
    while [[ ${#seeds[@]} -lt $num_seeds ]]; do
        seeds+=($RANDOM$RANDOM)
    done

    local pass=0 fail=0 failures=()
    local total="${num_seeds}"

    for i in $(seq 0 $((total - 1))); do
        local seed="${seeds[$i]}"
        local start_ms
        start_ms=$(date +%s%N 2>/dev/null || echo 0)

        if HARNESS_SEED="$seed" cargo test -p mcp-agent-mail-core \
            -p mcp-agent-mail-server -p mcp-agent-mail-db \
            "$test_name" -- --nocapture >/dev/null 2>&1; then
            pass=$((pass + 1))
            echo -e "  ${GREEN}PASS${NC} seed=$seed"
        else
            fail=$((fail + 1))
            failures+=("$seed")
            echo -e "  ${RED}FAIL${NC} seed=$seed"
        fi
    done

    echo ""
    echo -e "${BOLD}Results:${NC} $pass/$total passed, $fail/$total failed"

    if [[ $fail -eq 0 ]]; then
        ok "Test is STABLE across $total seeds."
    elif [[ $pass -eq 0 ]]; then
        error "Test ALWAYS FAILS — deterministic bug."
        echo "  Reproduce: HARNESS_SEED=${failures[0]} cargo test $test_name -- --nocapture"
    else
        local rate
        rate=$(python3 -c "print(f'{$fail/$total*100:.1f}%')" 2>/dev/null || echo "?")
        warn "Test is FLAKY — $rate failure rate."
        echo "  Failing seeds: ${failures[*]}"
        echo "  Reproduce: HARNESS_SEED=${failures[0]} cargo test $test_name -- --nocapture"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────

if [[ $# -eq 0 ]] || [[ "$1" == "--help" ]] || [[ "$1" == "-h" ]]; then
    usage
fi

case "$1" in
    --scan)
        shift
        scan_failures "${1:-}"
        ;;
    --multi-seed)
        shift
        if [[ $# -eq 0 ]]; then
            error "Usage: --multi-seed <test_name> [N]"
            exit 1
        fi
        multi_seed_run "$@"
        ;;
    *)
        reproduce_from_artifact "$1"
        ;;
esac
