#!/usr/bin/env bash
# br-3vwi.9.3: Unified soak/stress replay harness E2E script.
#
# One-command entry point for running multi-project soak tests with
# configurable parameters and CI-friendly artifact output.
#
# Usage:
#   tests/e2e/test_soak_harness.sh                    # Quick smoke (30s)
#   tests/e2e/test_soak_harness.sh --extended          # Extended run (300s)
#   tests/e2e/test_soak_harness.sh --stress            # Heavy stress (10p×20a, 200 RPS)
#   SOAK_SEED=42 tests/e2e/test_soak_harness.sh       # Deterministic replay
#
# Artifact output: tests/artifacts/soak/ (JSON reports + time-series)
#
# Exit codes:
#   0 = all thresholds pass
#   1 = one or more thresholds failed (see artifact for details)
#   2 = build or setup failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ── Source E2E library if available ──
if [[ -f "$REPO_ROOT/scripts/e2e_lib.sh" ]]; then
    # shellcheck source=/dev/null
    source "$REPO_ROOT/scripts/e2e_lib.sh"
    _E2E_SUITE="soak_harness"
fi

# ── Defaults ──
SOAK_SEED="${SOAK_SEED:-0}"
SOAK_PROJECTS="${SOAK_PROJECTS:-5}"
SOAK_AGENTS_PER_PROJECT="${SOAK_AGENTS_PER_PROJECT:-5}"
SUSTAINED_LOAD_RPS="${SUSTAINED_LOAD_RPS:-100}"
SUSTAINED_LOAD_SECS="${SUSTAINED_LOAD_SECS:-30}"
SOAK_DURATION_SECS="${SOAK_DURATION_SECS:-30}"

# ── Parse CLI args ──
PROFILE="quick"
for arg in "$@"; do
    case "$arg" in
        --extended)
            PROFILE="extended"
            SUSTAINED_LOAD_SECS=300
            SOAK_DURATION_SECS=300
            SOAK_PROJECTS=10
            SOAK_AGENTS_PER_PROJECT=10
            ;;
        --stress)
            PROFILE="stress"
            SUSTAINED_LOAD_SECS=120
            SOAK_DURATION_SECS=120
            SUSTAINED_LOAD_RPS=200
            SOAK_PROJECTS=10
            SOAK_AGENTS_PER_PROJECT=20
            ;;
        --quick)
            PROFILE="quick"
            ;;
        --help|-h)
            echo "Usage: $0 [--quick|--extended|--stress]"
            echo ""
            echo "Profiles:"
            echo "  --quick     30s, 5p×5a, 100 RPS (default)"
            echo "  --extended  300s, 10p×10a, 100 RPS"
            echo "  --stress    120s, 10p×20a, 200 RPS"
            echo ""
            echo "Environment overrides:"
            echo "  SOAK_SEED                 Deterministic seed (default: 0)"
            echo "  SOAK_PROJECTS             Number of projects (default: varies by profile)"
            echo "  SOAK_AGENTS_PER_PROJECT   Agents per project (default: varies by profile)"
            echo "  SUSTAINED_LOAD_RPS        Target RPS (default: varies by profile)"
            echo "  SUSTAINED_LOAD_SECS       Duration seconds (default: varies by profile)"
            exit 0
            ;;
    esac
done

export SOAK_SEED SOAK_PROJECTS SOAK_AGENTS_PER_PROJECT SUSTAINED_LOAD_RPS SUSTAINED_LOAD_SECS SOAK_DURATION_SECS
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/tmp/target-$(whoami)-am}"

echo "=== Soak Harness E2E (profile=$PROFILE) ==="
echo "  seed=$SOAK_SEED projects=$SOAK_PROJECTS agents/project=$SOAK_AGENTS_PER_PROJECT"
echo "  rps=$SUSTAINED_LOAD_RPS duration=${SUSTAINED_LOAD_SECS}s"
echo ""

PASS=0
FAIL=0

# ── Phase 1: Build ──
echo "--- Phase 1: Building test binaries ---"
if ! cargo test -p mcp-agent-mail-db --test sustained_load --no-run 2>&1; then
    echo "FAIL: Build failed"
    exit 2
fi
if ! cargo test -p mcp-agent-mail-server --test tui_soak_replay --no-run 2>&1; then
    echo "FAIL: Build failed"
    exit 2
fi
echo "BUILD OK"
echo ""

# ── Phase 2: DB Multi-Project Soak ──
echo "--- Phase 2: Multi-project DB soak (${SUSTAINED_LOAD_SECS}s) ---"
if cargo test -p mcp-agent-mail-db --test sustained_load multi_project_soak -- --ignored --nocapture 2>&1; then
    echo "PASS: multi_project_soak_replay"
    PASS=$((PASS + 1))
else
    echo "FAIL: multi_project_soak_replay"
    FAIL=$((FAIL + 1))
fi
echo ""

# ── Phase 3: TUI Soak Replay ──
echo "--- Phase 3: TUI soak replay (${SOAK_DURATION_SECS}s) ---"
if cargo test -p mcp-agent-mail-server --test tui_soak_replay soak_replay_empty_state -- --ignored --nocapture 2>&1; then
    echo "PASS: soak_replay_empty_state"
    PASS=$((PASS + 1))
else
    echo "FAIL: soak_replay_empty_state"
    FAIL=$((FAIL + 1))
fi
echo ""

# ── Phase 4: TUI Rapid Screen Cycling (non-ignored, always runs) ──
echo "--- Phase 4: TUI rapid screen cycling ---"
if cargo test -p mcp-agent-mail-server --test tui_soak_replay soak_rapid_screen_cycling -- --nocapture 2>&1; then
    echo "PASS: soak_rapid_screen_cycling"
    PASS=$((PASS + 1))
else
    echo "FAIL: soak_rapid_screen_cycling"
    FAIL=$((FAIL + 1))
fi
echo ""

# ── Phase 5: TUI Per-Screen Stability ──
echo "--- Phase 5: TUI per-screen stability ---"
if cargo test -p mcp-agent-mail-server --test tui_soak_replay soak_per_screen_stability -- --nocapture 2>&1; then
    echo "PASS: soak_per_screen_stability"
    PASS=$((PASS + 1))
else
    echo "FAIL: soak_per_screen_stability"
    FAIL=$((FAIL + 1))
fi
echo ""

# ── Phase 6: TUI Degradation Check ──
echo "--- Phase 6: TUI degradation check ---"
if cargo test -p mcp-agent-mail-server --test tui_soak_replay soak_no_degradation -- --nocapture 2>&1; then
    echo "PASS: soak_no_degradation"
    PASS=$((PASS + 1))
else
    echo "FAIL: soak_no_degradation"
    FAIL=$((FAIL + 1))
fi
echo ""

# ── Summary ──
TOTAL=$((PASS + FAIL))
echo "=== Soak Harness Summary ==="
echo "  Profile:  $PROFILE"
echo "  Passed:   $PASS / $TOTAL"
echo "  Failed:   $FAIL / $TOTAL"
echo "  Seed:     $SOAK_SEED"

if [[ "$FAIL" -gt 0 ]]; then
    echo ""
    echo "FAIL: $FAIL tests failed. Check artifacts in tests/artifacts/soak/"
    exit 1
fi

echo ""
echo "ALL PASS"
exit 0
