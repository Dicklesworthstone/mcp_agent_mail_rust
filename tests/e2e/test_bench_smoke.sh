#!/usr/bin/env bash
# test_bench_smoke.sh - Bench smoke test: validates golden outputs + basic budgets.
#
# Runs as part of `am e2e run --project . bench_smoke` (no network required).
# Compatibility fallback: `AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh bench_smoke`.
#
# Tests:
# 1. Golden output validation (all checksums match)
# 2. CLI startup budget (am --help < 100ms)
# 3. Stub encoder budget (< 50ms)
# 4. Criterion benchmark compilation (build check only)

E2E_SUITE="bench_smoke"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Bench Smoke Test Suite"

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"
AM="${CARGO_TARGET_DIR}/debug/am"

# ---------------------------------------------------------------------------
# Case 1: Golden output validation
# ---------------------------------------------------------------------------
e2e_case_banner "Golden output validation"

GOLDEN_DIR="${E2E_PROJECT_ROOT}/benches/golden"
if [ -f "${GOLDEN_DIR}/checksums.sha256" ]; then
    set +e
    golden_result=$("$AM" golden validate 2>&1)
    golden_rc=$?
    set -e

    if [ "$golden_rc" -eq 0 ]; then
        e2e_pass "All golden outputs match checksums"
    else
        e2e_fail "Golden output mismatch"
        echo "$golden_result" | tail -20
    fi

    e2e_save_artifact "golden_validation.txt" "$golden_result"
else
    e2e_skip "No golden checksums found (run 'am golden capture' first)"
fi

# ---------------------------------------------------------------------------
# Case 2: CLI startup budget (am --help < 100ms)
# ---------------------------------------------------------------------------
e2e_case_banner "CLI startup budget"

if [ -f "$AM" ]; then
    # Measure am --help execution time
    START_NS=$(date +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time_ns()))")
    "$AM" --help > /dev/null 2>&1
    END_NS=$(date +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time_ns()))")

    ELAPSED_MS=$(( (END_NS - START_NS) / 1000000 ))
    BUDGET_MS=100

    if [ "$ELAPSED_MS" -lt "$BUDGET_MS" ]; then
        e2e_pass "am --help: ${ELAPSED_MS}ms < ${BUDGET_MS}ms budget"
    else
        e2e_fail "am --help: ${ELAPSED_MS}ms exceeds ${BUDGET_MS}ms budget"
    fi

    e2e_save_artifact "cli_startup_ms.txt" "$ELAPSED_MS"
else
    e2e_skip "am binary not found at ${AM}"
fi

# ---------------------------------------------------------------------------
# Case 3: Stub encoder budget (< 50ms)
# ---------------------------------------------------------------------------
e2e_case_banner "Stub encoder budget"

STUB="${E2E_PROJECT_ROOT}/scripts/toon_stub_encoder.sh"
if [ -x "$STUB" ]; then
    START_NS=$(date +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time_ns()))")
    echo '{"id":1}' | "$STUB" --encode > /dev/null 2>&1
    END_NS=$(date +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time_ns()))")

    ELAPSED_MS=$(( (END_NS - START_NS) / 1000000 ))
    BUDGET_MS=50

    if [ "$ELAPSED_MS" -lt "$BUDGET_MS" ]; then
        e2e_pass "stub encoder: ${ELAPSED_MS}ms < ${BUDGET_MS}ms budget"
    else
        e2e_fail "stub encoder: ${ELAPSED_MS}ms exceeds ${BUDGET_MS}ms budget"
    fi

    e2e_save_artifact "stub_encoder_ms.txt" "$ELAPSED_MS"
else
    e2e_skip "Stub encoder not found"
fi

# ---------------------------------------------------------------------------
# Case 4: Criterion bench compilation check
# ---------------------------------------------------------------------------
e2e_case_banner "Criterion bench compilation"

set +e
cargo bench -p mcp-agent-mail-core --bench toon_bench --no-run 2>"${E2E_ARTIFACT_DIR}/bench_compile_stderr.txt"
bench_rc=$?
set -e

if [ "$bench_rc" -eq 0 ]; then
    e2e_pass "toon_bench compiles successfully"
else
    e2e_fail "toon_bench compilation failed"
    tail -10 "${E2E_ARTIFACT_DIR}/bench_compile_stderr.txt"
fi

# ---------------------------------------------------------------------------
# Case 5: BUDGETS.md exists and has expected sections
# ---------------------------------------------------------------------------
e2e_case_banner "Budget doc validation"

BUDGETS="${E2E_PROJECT_ROOT}/benches/BUDGETS.md"
if [ -f "$BUDGETS" ]; then
    e2e_pass "BUDGETS.md exists"

    budgets_content=$(cat "$BUDGETS")
    e2e_assert_contains "has Tool Handler Budgets section" "$budgets_content" "Tool Handler Budgets"
    e2e_assert_contains "has CLI Startup Budgets section" "$budgets_content" "CLI Startup Budgets"
    e2e_assert_contains "has Golden Outputs section" "$budgets_content" "Golden Outputs"
    e2e_assert_contains "has Optimization Workflow" "$budgets_content" "Optimization Workflow"
    e2e_assert_contains "has Isomorphism Invariants" "$budgets_content" "Isomorphism Invariants"
else
    e2e_fail "BUDGETS.md not found at ${BUDGETS}"
fi

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
