#!/usr/bin/env bash
# test_tick_strategy.sh - E2E coverage for TUI tick-strategy configuration
# (Predictive Screen Tick Management, frankentui bd-1nl75 H.3/I.7).
#
# Verifies, against the real `am` binary with a hermetic storage root:
# - `am robot health --format json` exposes the resolved tick strategy in
#   the `tui_tick` section (operator-verifiable without a TUI session)
# - the default strategy is Predictive with the tuned fallback divisor
# - AM_TUI_TICK_STRATEGY selects uniform / active_only / adjacent / predictive
# - AM_TUI_TICK_DIVISOR / _PERSIST / _MIN_OBSERVATIONS / _DECAY_FACTOR are
#   honored, with invalid values falling back safely
# - unknown strategy names fall back to Predictive (never a broken TUI)

set -euo pipefail

E2E_SUITE="tick_strategy"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI Tick Strategy E2E Test Suite"

# ---------------------------------------------------------------------------
# Prerequisites
# ---------------------------------------------------------------------------

if ! command -v jq >/dev/null 2>&1; then
    e2e_log "jq not found; skipping suite"
    e2e_skip "jq required for JSON validation"
    e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
    e2e_summary
    exit 0
fi

# Build the am binary
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

# ---------------------------------------------------------------------------
# Setup: hermetic storage/database so health probes never touch real state
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_tick_strategy")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
mkdir -p "${STORAGE_ROOT}"

export DATABASE_URL="sqlite:///${DB_PATH}"
export STORAGE_ROOT
export AM_INTERFACE_MODE=cli
# The suite controls every tick var explicitly per case; scrub any ambient
# operator configuration so assertions are deterministic.
unset AM_TUI_TICK_STRATEGY AM_TUI_TICK_DIVISOR AM_TUI_TICK_PERSIST \
    AM_TUI_TICK_MIN_OBSERVATIONS AM_TUI_TICK_DECAY_FACTOR || true

e2e_log "Work directory: ${WORK}"

# Run `am robot health --format json` with the given extra env assignments
# (as KEY=VALUE strings) and print the .tui_tick section.
tick_section() {
    env "$@" am robot health --format json 2>/dev/null | jq -c '.tui_tick'
}

tick_field() {
    local field="$1"
    shift
    tick_section "$@" | jq -r ".${field}"
}

# ---------------------------------------------------------------------------
# Case 1: health JSON exposes the tui_tick section with all fields
# ---------------------------------------------------------------------------

e2e_case_banner "health JSON exposes tui_tick section"

SECTION="$(tick_section)"
e2e_save_artifact "tui_tick_default.json" "${SECTION}"
for field in strategy configured divisor persist min_observations decay_factor; do
    if [ "$(printf '%s' "${SECTION}" | jq "has(\"${field}\")")" = "true" ]; then
        e2e_pass "tui_tick.${field} present"
    else
        e2e_fail "tui_tick.${field} missing from health JSON: ${SECTION}"
    fi
done

# ---------------------------------------------------------------------------
# Case 2: default strategy is Predictive with the tuned fallback cadence
# ---------------------------------------------------------------------------

e2e_case_banner "default strategy is Predictive"

e2e_assert_eq "default strategy label" "Predictive" "$(tick_field strategy)"
e2e_assert_eq "default configured string" "predictive" "$(tick_field configured)"
e2e_assert_eq "default fallback divisor" "12" "$(tick_field divisor)"
e2e_assert_eq "default persistence" "true" "$(tick_field persist)"
e2e_assert_eq "default min observations" "20" "$(tick_field min_observations)"

# ---------------------------------------------------------------------------
# Case 3: each strategy string selects the right strategy
# ---------------------------------------------------------------------------

e2e_case_banner "strategy selection via AM_TUI_TICK_STRATEGY"

e2e_assert_eq "uniform selects Uniform" "Uniform" \
    "$(tick_field strategy AM_TUI_TICK_STRATEGY=uniform)"
e2e_assert_eq "active_only selects ActiveOnly" "ActiveOnly" \
    "$(tick_field strategy AM_TUI_TICK_STRATEGY=active_only)"
e2e_assert_eq "adjacent selects ActivePlusAdjacent" "ActivePlusAdjacent" \
    "$(tick_field strategy AM_TUI_TICK_STRATEGY=adjacent)"
e2e_assert_eq "predictive selects Predictive" "Predictive" \
    "$(tick_field strategy AM_TUI_TICK_STRATEGY=predictive)"
e2e_assert_eq "strategy strings are case-insensitive" "Uniform" \
    "$(tick_field strategy AM_TUI_TICK_STRATEGY=UNIFORM)"

# ---------------------------------------------------------------------------
# Case 4: invalid strategy falls back to Predictive
# ---------------------------------------------------------------------------

e2e_case_banner "invalid strategy falls back to Predictive"

e2e_assert_eq "unknown strategy label falls back" "Predictive" \
    "$(tick_field strategy AM_TUI_TICK_STRATEGY=warp_speed)"
e2e_assert_eq "unknown strategy keeps configured=predictive" "predictive" \
    "$(tick_field configured AM_TUI_TICK_STRATEGY=warp_speed)"

# ---------------------------------------------------------------------------
# Case 5: divisor / persistence / predictor knobs are honored
# ---------------------------------------------------------------------------

e2e_case_banner "tick knobs are honored"

e2e_assert_eq "divisor override" "10" \
    "$(tick_field divisor AM_TUI_TICK_STRATEGY=uniform AM_TUI_TICK_DIVISOR=10)"
e2e_assert_eq "divisor 0 clamps to 1" "1" \
    "$(tick_field divisor AM_TUI_TICK_DIVISOR=0)"
e2e_assert_eq "persistence can be disabled" "false" \
    "$(tick_field persist AM_TUI_TICK_PERSIST=false)"
e2e_assert_eq "min observations override" "50" \
    "$(tick_field min_observations AM_TUI_TICK_MIN_OBSERVATIONS=50)"
e2e_assert_eq "decay factor override" "0.5" \
    "$(tick_field decay_factor AM_TUI_TICK_DECAY_FACTOR=0.5)"
e2e_assert_eq "out-of-range decay factor keeps default" "0.85" \
    "$(tick_field decay_factor AM_TUI_TICK_DECAY_FACTOR=7.5)"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "tui_tick_uniform.json" \
    "$(tick_section AM_TUI_TICK_STRATEGY=uniform AM_TUI_TICK_DIVISOR=10)"
e2e_summary
