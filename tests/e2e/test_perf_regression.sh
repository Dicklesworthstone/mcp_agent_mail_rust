#!/usr/bin/env bash
# test_perf_regression.sh - Performance Regression Suite (br-1xt0m.1.13.14)
#
# Exercises TUI frame render, action dispatch, and memory guardrails via the
# existing Rust test harness, captures structured JSON artifacts, and validates
# budget compliance from the E2E layer.
#
# Scope:
#   - Frame render p50/p95/p99 under representative load
#   - Action dispatch latency under burst operations
#   - Allocation churn and memory growth guardrails
#   - Environment metadata and reproducibility parameters
#
# Requires: jq, python3 (for JSON extraction), cargo

E2E_SUITE="perf_regression"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Performance Regression E2E Suite (br-1xt0m.1.13.14)"

# ── Prerequisites ────────────────────────────────────────────────────

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq not found; skipping perf regression suite"
    e2e_summary
    exit 0
fi

if ! command -v python3 >/dev/null 2>&1; then
    e2e_skip "python3 not found; skipping perf regression suite"
    e2e_summary
    exit 0
fi

CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"
export CARGO_TARGET_DIR
PROJECT_ROOT="${E2E_PROJECT_ROOT:-$(cd "${SCRIPT_DIR}/../.." && pwd)}"

# ── Helpers ──────────────────────────────────────────────────────────

now_ns() {
    date +%s%N 2>/dev/null || python3 -c "import time; print(int(time.time_ns()))"
}

# Find the most recent JSON artifact under a directory tree.
find_latest_artifact() {
    local base_dir="$1" name="$2"
    find "$base_dir" -name "$name" -type f 2>/dev/null | sort -r | head -1
}

# Extract a numeric field from a JSON file using jq.
jq_num() {
    local file="$1" path="$2"
    jq -r "$path // empty" "$file" 2>/dev/null
}

# Save env metadata for reproducibility.
save_env_metadata() {
    local dest="$1"
    python3 - "$dest" <<'PY'
import json, os, sys, platform, subprocess

def cmd(args):
    try:
        return subprocess.check_output(args, stderr=subprocess.DEVNULL, timeout=5).decode().strip()
    except Exception:
        return "unknown"

meta = {
    "hostname": platform.node(),
    "arch": platform.machine(),
    "os": platform.system(),
    "os_release": platform.release(),
    "cpus": os.cpu_count(),
    "cargo_target_dir": os.environ.get("CARGO_TARGET_DIR", ""),
    "build_profile": os.environ.get("PERF_BUILD_PROFILE", "debug"),
    "rustc_version": cmd(["rustc", "--version"]),
    "git_sha": cmd(["git", "-C", os.environ.get("E2E_PROJECT_ROOT", "."), "rev-parse", "--short", "HEAD"]),
    "git_dirty": cmd(["git", "-C", os.environ.get("E2E_PROJECT_ROOT", "."), "diff", "--stat"]) != "",
    "load_avg": list(os.getloadavg()),
}

with open(sys.argv[1], "w") as f:
    json.dump(meta, f, indent=2)
    f.write("\n")
PY
}

# ── Case 1: Build perf test binaries ────────────────────────────────

e2e_case_banner "Build perf test binaries"
e2e_step_start "build_perf_tests"

BUILD_LOG="${E2E_ARTIFACT_DIR}/diagnostics/build_perf.log"
mkdir -p "$(dirname "$BUILD_LOG")"

set +e
cargo test -p mcp-agent-mail-server --test tui_perf_baselines --no-run \
    2>"$BUILD_LOG"
build_rc=$?
set -e

e2e_step_end "build_perf_tests"

if [ "$build_rc" -ne 0 ]; then
    e2e_fail "perf baselines test binary failed to compile"
    tail -20 "$BUILD_LOG" >&2 || true
    e2e_save_artifact "diagnostics/build_perf.log" "$(cat "$BUILD_LOG")"
    e2e_summary
    exit 1
fi
e2e_pass "perf baselines test binary compiles"

# ── Case 2: Run frame render + action latency baselines ─────────────

e2e_case_banner "Frame render and action dispatch baselines"
e2e_step_start "run_perf_baselines"

# Clean previous artifacts so find_latest_artifact gets a fresh one.
PERF_ARTIFACT_BASE="${PROJECT_ROOT}/tests/artifacts/tui/perf_baselines"
PRE_RUN_MARKER="$(date +%s)"

BASELINE_OUTPUT="${E2E_ARTIFACT_DIR}/perf_baselines_stdout.txt"
BASELINE_STDERR="${E2E_ARTIFACT_DIR}/perf_baselines_stderr.txt"

set +e
MCP_AGENT_MAIL_BENCH_ENFORCE_BUDGETS=1 \
    cargo test -p mcp-agent-mail-server --test tui_perf_baselines \
    -- z_perf_baseline_report --nocapture \
    >"$BASELINE_OUTPUT" 2>"$BASELINE_STDERR"
baseline_rc=$?
set -e

e2e_step_end "run_perf_baselines"

e2e_save_artifact "perf_baselines_stderr.txt" "$(cat "$BASELINE_STDERR" 2>/dev/null)"

# Find the generated summary.json artifact.
SUMMARY_JSON="$(find_latest_artifact "$PERF_ARTIFACT_BASE" "summary.json")"

if [ -z "$SUMMARY_JSON" ] || [ ! -f "$SUMMARY_JSON" ]; then
    e2e_fail "perf baselines did not produce summary.json artifact"
    e2e_summary
    exit 1
fi

e2e_copy_artifact "$SUMMARY_JSON" "perf_summary.json"

# Parse and validate.
ALL_WITHIN="$(jq_num "$SUMMARY_JSON" '.all_within_budget')"
SAMPLE_COUNT="$(jq '.samples | length' "$SUMMARY_JSON" 2>/dev/null)"

if [ "$ALL_WITHIN" = "true" ]; then
    e2e_pass "all ${SAMPLE_COUNT} perf samples within budget"
else
    e2e_fail "perf budget exceeded (see perf_summary.json artifact)"
fi

if [ "$baseline_rc" -eq 0 ]; then
    e2e_pass "perf baseline test suite exited cleanly"
else
    e2e_fail "perf baseline test suite failed (rc=${baseline_rc})"
fi

# ── Case 3: Validate frame render p50/p95/p99 ranges ────────────────

e2e_case_banner "Frame render latency validation"

python3 - "$SUMMARY_JSON" <<'PY'
import json, sys, os

path = sys.argv[1]
with open(path) as f:
    report = json.load(f)

# Budgets in microseconds (must match tui_perf_baselines.rs)
BUDGETS = {
    "screen_render": 10_000,   # p95 < 10ms
    "app_render":    15_000,   # p95 < 15ms
    "tick_cycle":    20_000,   # p95 < 20ms
}

exit_code = 0
results = []

for sample in report.get("samples", []):
    surface = sample["surface"]
    p50 = sample["p50_us"]
    p95 = sample["p95_us"]
    p99 = sample["p99_us"]
    max_us = sample["max_us"]
    budget = sample.get("budget_p95_us", 0)
    within = sample.get("within_budget", False)

    results.append({
        "surface": surface,
        "detail": sample.get("detail", ""),
        "p50_us": p50,
        "p95_us": p95,
        "p99_us": p99,
        "max_us": max_us,
        "budget_p95_us": budget,
        "within_budget": within,
    })

    # Sanity: p50 <= p95 <= p99 <= max
    if not (p50 <= p95 <= p99 <= max_us):
        print(f"ERROR: {surface}: percentile ordering violated: "
              f"p50={p50} p95={p95} p99={p99} max={max_us}", file=sys.stderr)
        exit_code = 1

    # p50 should be strictly positive
    if p50 == 0:
        print(f"WARNING: {surface}: p50 is 0 (measurement granularity?)", file=sys.stderr)

# Write detailed results for artifact
artifact_dir = os.environ.get("E2E_ARTIFACT_DIR", "/tmp")
with open(os.path.join(artifact_dir, "frame_render_analysis.json"), "w") as f:
    json.dump(results, f, indent=2)
    f.write("\n")

# Count render-related samples
render_surfaces = [r for r in results if r["surface"] in ("screen_render", "app_render")]
if len(render_surfaces) == 0:
    print("ERROR: no screen_render or app_render samples found", file=sys.stderr)
    exit_code = 1

sys.exit(exit_code)
PY
render_rc=$?

if [ "$render_rc" -eq 0 ]; then
    e2e_pass "frame render percentiles monotonic and non-degenerate"
else
    e2e_fail "frame render percentile validation failed"
fi

# Extract specific screen render p95 values for structured assertions.
SCREEN_RENDER_P95_MAX="$(jq '[.samples[] | select(.surface=="screen_render") | .p95_us] | max // 0' "$SUMMARY_JSON")"
APP_RENDER_P95="$(jq '[.samples[] | select(.surface=="app_render") | .p95_us][0] // 0' "$SUMMARY_JSON")"

if [ "${SCREEN_RENDER_P95_MAX:-0}" -gt 0 ] && [ "$SCREEN_RENDER_P95_MAX" -lt 10000 ]; then
    e2e_pass "screen render p95 max across all screens: ${SCREEN_RENDER_P95_MAX}us < 10ms"
elif [ "${SCREEN_RENDER_P95_MAX:-0}" -eq 0 ]; then
    e2e_skip "no screen_render samples found"
else
    e2e_fail "screen render p95 max: ${SCREEN_RENDER_P95_MAX}us exceeds 10ms budget"
fi

if [ "${APP_RENDER_P95:-0}" -gt 0 ] && [ "$APP_RENDER_P95" -lt 15000 ]; then
    e2e_pass "app render p95: ${APP_RENDER_P95}us < 15ms"
elif [ "${APP_RENDER_P95:-0}" -eq 0 ]; then
    e2e_skip "no app_render samples found"
else
    e2e_fail "app render p95: ${APP_RENDER_P95}us exceeds 15ms budget"
fi

# ── Case 4: Action dispatch latency validation ──────────────────────

e2e_case_banner "Action dispatch latency validation"

# Extract samples that exist in the combined z_perf_baseline_report.
# Surfaces: model_init, tick_update, screen_render, app_render, tick_cycle,
#           search_interaction, key_navigation.
# Note: p95_us can be 0 on fast hardware (sub-microsecond operations) — that's within budget.
TICK_UPDATE_P95="$(jq '[.samples[] | select(.surface=="tick_update") | .p95_us][0] // "absent"' "$SUMMARY_JSON")"
TICK_CYCLE_P95="$(jq '[.samples[] | select(.surface=="tick_cycle") | .p95_us][0] // "absent"' "$SUMMARY_JSON")"
SEARCH_P95="$(jq '[.samples[] | select(.surface=="search_interaction") | .p95_us][0] // "absent"' "$SUMMARY_JSON")"
KEY_NAV_P95="$(jq '[.samples[] | select(.surface=="key_navigation") | .p95_us][0] // "absent"' "$SUMMARY_JSON")"

if [ "$TICK_UPDATE_P95" != "absent" ] && [ "$TICK_UPDATE_P95" -lt 2000 ]; then
    e2e_pass "tick update p95: ${TICK_UPDATE_P95}us < 2ms"
elif [ "$TICK_UPDATE_P95" = "absent" ]; then
    e2e_skip "no tick_update samples"
else
    e2e_fail "tick update p95: ${TICK_UPDATE_P95}us exceeds 2ms budget"
fi

if [ "$TICK_CYCLE_P95" != "absent" ] && [ "$TICK_CYCLE_P95" -lt 20000 ]; then
    e2e_pass "tick cycle p95: ${TICK_CYCLE_P95}us < 20ms (well under 100ms interval)"
elif [ "$TICK_CYCLE_P95" = "absent" ]; then
    e2e_skip "no tick_cycle samples"
else
    e2e_fail "tick cycle p95: ${TICK_CYCLE_P95}us exceeds 20ms budget"
fi

if [ "$SEARCH_P95" != "absent" ] && [ "$SEARCH_P95" -lt 2000 ]; then
    e2e_pass "search interaction p95: ${SEARCH_P95}us < 2ms"
elif [ "$SEARCH_P95" = "absent" ]; then
    e2e_skip "no search_interaction samples"
else
    e2e_fail "search interaction p95: ${SEARCH_P95}us exceeds 2ms budget"
fi

if [ "$KEY_NAV_P95" != "absent" ] && [ "$KEY_NAV_P95" -lt 2000 ]; then
    e2e_pass "key navigation p95: ${KEY_NAV_P95}us < 2ms"
elif [ "$KEY_NAV_P95" = "absent" ]; then
    e2e_skip "no key_navigation samples"
else
    e2e_fail "key navigation p95: ${KEY_NAV_P95}us exceeds 2ms budget"
fi

# ── Case 5: Build soak test binary ──────────────────────────────────

e2e_case_banner "Build and run soak replay"
e2e_step_start "build_soak_test"

SOAK_BUILD_LOG="${E2E_ARTIFACT_DIR}/diagnostics/build_soak.log"
set +e
cargo test -p mcp-agent-mail-server --test tui_soak_replay --no-run \
    2>"$SOAK_BUILD_LOG"
soak_build_rc=$?
set -e

e2e_step_end "build_soak_test"

if [ "$soak_build_rc" -ne 0 ]; then
    e2e_fail "soak replay test binary failed to compile"
    tail -20 "$SOAK_BUILD_LOG" >&2 || true
    e2e_summary
    exit 1
fi
e2e_pass "soak replay test binary compiles"

# Run a short soak (10 seconds for E2E; real CI can use SOAK_DURATION_SECS=300).
e2e_step_start "run_soak_replay"

SOAK_ARTIFACT_BASE="${PROJECT_ROOT}/tests/artifacts/tui/soak_replay"
SOAK_OUTPUT="${E2E_ARTIFACT_DIR}/soak_stdout.txt"
SOAK_STDERR="${E2E_ARTIFACT_DIR}/soak_stderr.txt"

set +e
SOAK_DURATION_SECS="${SOAK_DURATION_SECS:-10}" \
    cargo test -p mcp-agent-mail-server --test tui_soak_replay \
    -- soak_replay_empty_state --ignored --nocapture \
    >"$SOAK_OUTPUT" 2>"$SOAK_STDERR"
soak_rc=$?
set -e

e2e_step_end "run_soak_replay"

e2e_save_artifact "soak_stderr.txt" "$(cat "$SOAK_STDERR" 2>/dev/null)"

# ── Case 6: Memory growth guardrails ────────────────────────────────

e2e_case_banner "Memory growth guardrails"

SOAK_JSON="$(find_latest_artifact "$SOAK_ARTIFACT_BASE" "report.json")"

if [ -z "$SOAK_JSON" ] || [ ! -f "$SOAK_JSON" ]; then
    # Soak test may not produce artifact if it panics; still validate exit code.
    if [ "$soak_rc" -eq 0 ]; then
        e2e_pass "soak replay exited cleanly (no report.json — test may skip on this platform)"
    else
        e2e_fail "soak replay failed (rc=${soak_rc}) and produced no report.json"
    fi
    e2e_skip "no soak report.json artifact; skipping memory assertions"
else
    e2e_copy_artifact "$SOAK_JSON" "soak_report.json"

    VERDICT="$(jq_num "$SOAK_JSON" '.verdict')"
    RSS_GROWTH="$(jq_num "$SOAK_JSON" '.rss_growth_factor')"
    BASELINE_RSS="$(jq_num "$SOAK_JSON" '.baseline_rss_kb')"
    FINAL_RSS="$(jq_num "$SOAK_JSON" '.final_rss_kb')"
    SOAK_ERRORS="$(jq_num "$SOAK_JSON" '.errors')"
    ACTION_P95="$(jq_num "$SOAK_JSON" '.action_p95_us')"
    RENDER_P95="$(jq_num "$SOAK_JSON" '.render_p95_us')"

    if [ "$soak_rc" -eq 0 ]; then
        e2e_pass "soak replay exited cleanly"
    else
        e2e_fail "soak replay failed (rc=${soak_rc})"
    fi

    # Memory growth: must not exceed 3x baseline.
    if [ -n "$RSS_GROWTH" ]; then
        GROWTH_OK="$(python3 -c "print('yes' if float('$RSS_GROWTH') <= 3.0 else 'no')")"
        if [ "$GROWTH_OK" = "yes" ]; then
            e2e_pass "RSS growth factor: ${RSS_GROWTH}x <= 3.0x (baseline=${BASELINE_RSS}KB final=${FINAL_RSS}KB)"
        else
            e2e_fail "RSS growth factor: ${RSS_GROWTH}x exceeds 3.0x guardrail (baseline=${BASELINE_RSS}KB final=${FINAL_RSS}KB)"
        fi
    else
        e2e_skip "RSS growth not reported"
    fi

    # Error count during soak.
    if [ "${SOAK_ERRORS:-0}" -eq 0 ]; then
        e2e_pass "zero errors during soak replay"
    else
        e2e_fail "soak produced ${SOAK_ERRORS} error(s)"
    fi

    # Action p95 under soak budget (50ms).
    if [ -n "$ACTION_P95" ] && [ "$ACTION_P95" -lt 50000 ]; then
        e2e_pass "soak action p95: ${ACTION_P95}us < 50ms"
    elif [ -n "$ACTION_P95" ]; then
        e2e_fail "soak action p95: ${ACTION_P95}us exceeds 50ms"
    else
        e2e_skip "soak action p95 not reported"
    fi

    # Render p95 under soak budget (75ms).
    if [ -n "$RENDER_P95" ] && [ "$RENDER_P95" -lt 75000 ]; then
        e2e_pass "soak render p95: ${RENDER_P95}us < 75ms"
    elif [ -n "$RENDER_P95" ]; then
        e2e_fail "soak render p95: ${RENDER_P95}us exceeds 75ms"
    else
        e2e_skip "soak render p95 not reported"
    fi

    # Verdict from the Rust test itself.
    if [ "$VERDICT" = "PASS" ]; then
        e2e_pass "soak verdict: PASS"
    elif [ -n "$VERDICT" ]; then
        e2e_fail "soak verdict: ${VERDICT}"
    fi
fi

# ── Case 7: Artifact completeness and reproducibility ───────────────

e2e_case_banner "Artifact completeness and reproducibility metadata"

# Save environment metadata.
ENV_META="${E2E_ARTIFACT_DIR}/env_metadata.json"
save_env_metadata "$ENV_META"
e2e_copy_artifact "$ENV_META" "env_metadata.json"

if [ -f "$ENV_META" ]; then
    # Validate required fields.
    REQUIRED_FIELDS='["hostname","arch","cpus","cargo_target_dir","build_profile","rustc_version","git_sha"]'
    MISSING="$(python3 -c "
import json, sys
with open('$ENV_META') as f:
    meta = json.load(f)
required = $REQUIRED_FIELDS
missing = [k for k in required if k not in meta or meta[k] in (None, '')]
print(','.join(missing) if missing else '')
")"
    if [ -z "$MISSING" ]; then
        e2e_pass "env metadata has all required fields"
    else
        e2e_fail "env metadata missing: ${MISSING}"
    fi
else
    e2e_fail "failed to generate env_metadata.json"
fi

# Validate perf summary artifact structure.
if [ -n "$SUMMARY_JSON" ] && [ -f "$SUMMARY_JSON" ]; then
    STRUCT_OK="$(python3 -c "
import json, sys
with open('$SUMMARY_JSON') as f:
    r = json.load(f)
ok = True
for field in ('generated_at', 'agent', 'bead', 'build_profile', 'samples', 'all_within_budget'):
    if field not in r:
        print(f'missing: {field}', file=sys.stderr)
        ok = False
for s in r.get('samples', []):
    for sf in ('surface', 'detail', 'iterations', 'warmup', 'p50_us', 'p95_us', 'p99_us', 'max_us', 'budget_p95_us', 'within_budget'):
        if sf not in s:
            print(f'sample missing: {sf}', file=sys.stderr)
            ok = False
            break
print('ok' if ok else 'fail')
")"
    if [ "$STRUCT_OK" = "ok" ]; then
        e2e_pass "perf summary artifact has complete schema"
    else
        e2e_fail "perf summary artifact missing required fields"
    fi
else
    e2e_skip "no perf summary artifact to validate"
fi

# ── Case 8: Per-screen render budget compliance ─────────────────────

e2e_case_banner "Per-screen render budget compliance"

if [ -n "$SUMMARY_JSON" ] && [ -f "$SUMMARY_JSON" ]; then
    SCREEN_RESULTS="$(python3 - "$SUMMARY_JSON" <<'PY'
import json, sys

with open(sys.argv[1]) as f:
    report = json.load(f)

screen_samples = [s for s in report.get("samples", []) if s["surface"] == "screen_render"]
if not screen_samples:
    print("NONE")
    sys.exit(0)

all_ok = True
lines = []
for s in screen_samples:
    status = "OK" if s["within_budget"] else "OVER"
    lines.append(f"{s['detail']}: p95={s['p95_us']}us budget={s['budget_p95_us']}us {status}")
    if not s["within_budget"]:
        all_ok = False

# Print summary
for line in lines:
    print(line)

sys.exit(0 if all_ok else 1)
PY
)"
    screen_rc=$?

    if [ "$SCREEN_RESULTS" = "NONE" ]; then
        e2e_skip "no per-screen render samples in report"
    elif [ "$screen_rc" -eq 0 ]; then
        SCREEN_COUNT="$(echo "$SCREEN_RESULTS" | wc -l)"
        e2e_pass "all ${SCREEN_COUNT} screens within render budget"
    else
        e2e_fail "some screens exceeded render budget"
        echo "$SCREEN_RESULTS" | grep "OVER" >&2
    fi

    e2e_save_artifact "per_screen_render.txt" "$SCREEN_RESULTS"
else
    e2e_skip "no perf summary to validate per-screen budgets"
fi

# ── Summary ──────────────────────────────────────────────────────────

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
