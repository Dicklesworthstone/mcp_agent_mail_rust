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
# Artifact output:
#   - DB soak:  tests/artifacts/soak/multi_project/*/report.json
#   - TUI soak: tests/artifacts/tui/soak_replay/*/report.json
#
# Exit codes:
#   0 = all thresholds pass
#   1 = one or more thresholds failed (see artifact for details)
#   2 = build or setup failure

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

HAS_E2E_LIB=0

# ── Source E2E library if available ──
if [[ -f "$REPO_ROOT/scripts/e2e_lib.sh" ]]; then
    # Safety: default to keeping temp dirs so shared harness cleanup doesn't run `rm -rf`.
    : "${AM_E2E_KEEP_TMP:=1}"

    E2E_SUITE="soak_harness"
    # shellcheck source=/dev/null
    source "$REPO_ROOT/scripts/e2e_lib.sh"
    e2e_init_artifacts
    HAS_E2E_LIB=1
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

if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_banner "Soak Harness E2E (profile=$PROFILE)"
    e2e_log "seed=$SOAK_SEED projects=$SOAK_PROJECTS agents/project=$SOAK_AGENTS_PER_PROJECT"
    e2e_log "rps=$SUSTAINED_LOAD_RPS duration=${SUSTAINED_LOAD_SECS}s"
else
    echo "=== Soak Harness E2E (profile=$PROFILE) ==="
    echo "  seed=$SOAK_SEED projects=$SOAK_PROJECTS agents/project=$SOAK_AGENTS_PER_PROJECT"
    echo "  rps=$SUSTAINED_LOAD_RPS duration=${SUSTAINED_LOAD_SECS}s"
    echo ""
fi

PASS=0
FAIL=0

# ── Phase 1: Build ──
if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_case_banner "build"
else
    echo "--- Phase 1: Building test binaries ---"
fi
if ! cargo test -p mcp-agent-mail-db --test sustained_load --no-run 2>&1; then
    echo "FAIL: Build failed"
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_fail "build (mcp-agent-mail-db)"
        e2e_summary || true
    fi
    exit 2
fi
if ! cargo test -p mcp-agent-mail-server --test tui_soak_replay --no-run 2>&1; then
    echo "FAIL: Build failed"
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_fail "build (mcp-agent-mail-server)"
        e2e_summary || true
    fi
    exit 2
fi
echo "BUILD OK"
if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_pass "build"
fi
echo ""

# ── Phase 2: DB Multi-Project Soak ──
if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_case_banner "db multi-project soak (${SUSTAINED_LOAD_SECS}s)"
else
    echo "--- Phase 2: Multi-project DB soak (${SUSTAINED_LOAD_SECS}s) ---"
fi
if cargo test -p mcp-agent-mail-db --test sustained_load multi_project_soak_replay -- --ignored --nocapture 2>&1; then
    echo "PASS: multi_project_soak_replay"
    PASS=$((PASS + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_pass "multi_project_soak_replay"
    fi
else
    echo "FAIL: multi_project_soak_replay"
    FAIL=$((FAIL + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_fail "multi_project_soak_replay"
    fi
fi
echo ""

# ── Phase 3: TUI Soak Replay ──
if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_case_banner "tui soak replay (${SOAK_DURATION_SECS}s)"
else
    echo "--- Phase 3: TUI soak replay (${SOAK_DURATION_SECS}s) ---"
fi
if cargo test -p mcp-agent-mail-server --test tui_soak_replay soak_replay_empty_state -- --ignored --nocapture 2>&1; then
    echo "PASS: soak_replay_empty_state"
    PASS=$((PASS + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_pass "soak_replay_empty_state"
    fi
else
    echo "FAIL: soak_replay_empty_state"
    FAIL=$((FAIL + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_fail "soak_replay_empty_state"
    fi
fi
echo ""

# ── Phase 4: TUI Rapid Screen Cycling (non-ignored, always runs) ──
if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_case_banner "tui rapid screen cycling"
else
    echo "--- Phase 4: TUI rapid screen cycling ---"
fi
if cargo test -p mcp-agent-mail-server --test tui_soak_replay soak_rapid_screen_cycling -- --nocapture 2>&1; then
    echo "PASS: soak_rapid_screen_cycling"
    PASS=$((PASS + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_pass "soak_rapid_screen_cycling"
    fi
else
    echo "FAIL: soak_rapid_screen_cycling"
    FAIL=$((FAIL + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_fail "soak_rapid_screen_cycling"
    fi
fi
echo ""

# ── Phase 5: TUI Per-Screen Stability ──
if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_case_banner "tui per-screen stability"
else
    echo "--- Phase 5: TUI per-screen stability ---"
fi
if cargo test -p mcp-agent-mail-server --test tui_soak_replay soak_per_screen_stability -- --nocapture 2>&1; then
    echo "PASS: soak_per_screen_stability"
    PASS=$((PASS + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_pass "soak_per_screen_stability"
    fi
else
    echo "FAIL: soak_per_screen_stability"
    FAIL=$((FAIL + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_fail "soak_per_screen_stability"
    fi
fi
echo ""

# ── Phase 6: TUI Degradation Check ──
if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_case_banner "tui degradation check"
else
    echo "--- Phase 6: TUI degradation check ---"
fi
if cargo test -p mcp-agent-mail-server --test tui_soak_replay soak_no_degradation -- --nocapture 2>&1; then
    echo "PASS: soak_no_degradation"
    PASS=$((PASS + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_pass "soak_no_degradation"
    fi
else
    echo "FAIL: soak_no_degradation"
    FAIL=$((FAIL + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_fail "soak_no_degradation"
    fi
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
    echo "FAIL: $FAIL tests failed. Check artifacts under tests/artifacts/."
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_summary || true
    fi
    exit 1
fi

echo ""
echo "ALL PASS"
if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_summary
    exit $?
fi
exit 0
