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
#   - Trends:   tests/artifacts/perf/soak_harness/trends/perf_timeseries.jsonl
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

emit_soak_perf_trends() {
    local db_report
    local tui_report
    local rapid_report
    local search_report
    local trend_dir
    local trend_file
    local summary_file

    db_report="$(command find "$REPO_ROOT/tests/artifacts/soak/multi_project" -type f -name report.json -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -n1 | awk '{print $2}')"
    tui_report="$(command find "$REPO_ROOT/tests/artifacts/tui/soak_replay" -type f -name report.json -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -n1 | awk '{print $2}')"
    rapid_report="$(command find "$REPO_ROOT/tests/artifacts/tui/soak_replay" -type f -name rapid_screen_cycling_report.json -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -n1 | awk '{print $2}')"
    search_report="$(command find "$REPO_ROOT/tests/artifacts/tui/soak_replay" -type f -name search_typing_report.json -printf '%T@ %p\n' 2>/dev/null | sort -nr | head -n1 | awk '{print $2}')"

    if [[ -z "$db_report" || -z "$tui_report" || -z "$rapid_report" || -z "$search_report" ]]; then
        echo "Missing soak report(s) for trend synthesis."
        echo "  db_report=$db_report"
        echo "  tui_report=$tui_report"
        echo "  rapid_report=$rapid_report"
        echo "  search_report=$search_report"
        return 1
    fi

    trend_dir="$REPO_ROOT/tests/artifacts/perf/soak_harness/trends"
    trend_file="$trend_dir/perf_timeseries.jsonl"
    summary_file="$trend_dir/latest_summary.json"
    mkdir -p "$trend_dir"

    PROFILE="$PROFILE" SOAK_SEED="$SOAK_SEED" python3 - "$db_report" "$tui_report" "$rapid_report" "$search_report" "$trend_file" "$summary_file" <<'PY'
import datetime as dt
import hashlib
import json
import math
import os
import platform
import re
import sys
from pathlib import Path

db_path, tui_path, rapid_path, search_path, trend_path, summary_path = sys.argv[1:]

def load_json(path: str):
    with open(path, "r", encoding="utf-8") as f:
        return json.load(f)

def pct(sorted_vals, p):
    if not sorted_vals:
        return 0
    idx = round((p / 100.0) * (len(sorted_vals) - 1))
    idx = max(0, min(idx, len(sorted_vals) - 1))
    return int(sorted_vals[idx])

def mean(vals):
    if not vals:
        return 0.0
    return float(sum(vals)) / float(len(vals))

def variance(vals):
    if not vals:
        return 0.0
    m = mean(vals)
    return sum((float(v) - m) ** 2 for v in vals) / float(len(vals))

def stddev(vals):
    return math.sqrt(variance(vals))

def metric_env_suffix(metric):
    return re.sub(r"[^A-Z0-9]+", "_", metric.upper())

def parse_opt_int(name):
    raw = os.getenv(name)
    if raw is None or raw == "":
        return None
    return int(raw)

def baseline_for(metric):
    suffix = metric_env_suffix(metric)
    return parse_opt_int(f"SOAK_BASELINE_P95_US_{suffix}") or parse_opt_int("SOAK_BASELINE_P95_US")

def max_delta_for(metric):
    suffix = metric_env_suffix(metric)
    return parse_opt_int(f"SOAK_MAX_DELTA_P95_US_{suffix}") or parse_opt_int("SOAK_MAX_DELTA_P95_US")

def budget_for(metric, default):
    suffix = metric_env_suffix(metric)
    return parse_opt_int(f"SOAK_BUDGET_P95_US_{suffix}") or default

def fixture_signature(metric, profile, seed, source_path):
    payload = f"metric={metric};profile={profile};seed={seed};source={source_path}"
    return hashlib.sha1(payload.encode("utf-8")).hexdigest()[:16]

env_profile = {
    "os": platform.system(),
    "release": platform.release(),
    "machine": platform.machine(),
    "python": platform.python_version(),
    "cpu_count": os.cpu_count(),
}

profile = os.getenv("PROFILE", "quick")
seed = os.getenv("SOAK_SEED", "0")
generated_at = dt.datetime.now(dt.timezone.utc).isoformat()

db_report = load_json(db_path)
tui_report = load_json(tui_path)
rapid_report = load_json(rapid_path)
search_report = load_json(search_path)

rows = []
failures = []

def add_row(metric_name, category, samples, p50, p95, p99, max_us, budget_default, source_path, extra):
    sorted_samples = sorted(int(v) for v in samples if v is not None)
    if not sorted_samples:
        sorted_samples = [int(p95)]
    budget_p95 = budget_for(metric_name, budget_default)
    baseline = baseline_for(metric_name)
    max_delta = max_delta_for(metric_name)
    delta = (int(p95) - baseline) if baseline is not None else None
    budget_ok = int(p95) <= int(budget_p95) if budget_p95 is not None else True
    regression_ok = True
    if delta is not None and max_delta is not None:
        regression_ok = delta <= int(max_delta)
    passed = budget_ok and regression_ok
    row = {
        "generated_at": generated_at,
        "profile": profile,
        "seed": seed,
        "metric_name": metric_name,
        "category": category,
        "samples_us": sorted_samples,
        "p50_us": int(p50),
        "p95_us": int(p95),
        "p99_us": int(p99),
        "max_us": int(max_us),
        "mean_us": round(mean(sorted_samples), 3),
        "variance_us2": round(variance(sorted_samples), 3),
        "stddev_us": round(stddev(sorted_samples), 3),
        "budget_p95_us": budget_p95,
        "baseline_p95_us": baseline,
        "delta_p95_us": delta,
        "max_delta_p95_us": max_delta,
        "fixture_signature": fixture_signature(metric_name, profile, seed, source_path),
        "environment": env_profile,
        "passed": passed,
        "source_report": str(source_path),
    }
    row.update(extra)
    if not passed:
        failures.append(
            f"{metric_name}: p95={row['p95_us']} budget={row['budget_p95_us']} delta={row['delta_p95_us']} max_delta={row['max_delta_p95_us']}"
        )
    rows.append(row)

db_samples = [int(s.get("p95_us", 0)) for s in db_report.get("snapshots", [])]
add_row(
    metric_name="db_sustained_session",
    category="sustained",
    samples=db_samples,
    p50=db_report.get("p50_us", pct(sorted(db_samples), 50.0)),
    p95=db_report.get("p95_us", pct(sorted(db_samples), 95.0)),
    p99=db_report.get("p99_us", pct(sorted(db_samples), 99.0)),
    max_us=db_report.get("max_us", max(db_samples) if db_samples else 0),
    budget_default=1_500_000,
    source_path=db_path,
    extra={
        "target_rps": db_report.get("target_rps"),
        "actual_rps": db_report.get("actual_rps"),
        "thresholds": db_report.get("thresholds", {}),
        "verdict": db_report.get("verdict"),
    },
)

render_samples = [int(s.get("render_p95_us", 0)) for s in tui_report.get("snapshots", [])]
add_row(
    metric_name="tui_render_loop",
    category="render",
    samples=render_samples,
    p50=tui_report.get("render_p50_us", pct(sorted(render_samples), 50.0)),
    p95=tui_report.get("render_p95_us", pct(sorted(render_samples), 95.0)),
    p99=tui_report.get("render_p99_us", pct(sorted(render_samples), 99.0)),
    max_us=tui_report.get("render_max_us", max(render_samples) if render_samples else 0),
    budget_default=75_000,
    source_path=tui_path,
    extra={
        "replay_loops": tui_report.get("replay_loops"),
        "total_renders": tui_report.get("total_renders"),
        "verdict": tui_report.get("verdict"),
    },
)

add_row(
    metric_name="tui_interaction_rapid_screen_cycle",
    category="interaction",
    samples=rapid_report.get("samples_us", []),
    p50=rapid_report.get("p50_us", 0),
    p95=rapid_report.get("p95_us", 0),
    p99=rapid_report.get("p99_us", 0),
    max_us=rapid_report.get("max_us", 0),
    budget_default=50_000,
    source_path=rapid_path,
    extra={"verdict": "PASS" if rapid_report.get("passed", False) else "FAIL"},
)

add_row(
    metric_name="tui_search_typing",
    category="search",
    samples=search_report.get("samples_us", []),
    p50=search_report.get("p50_us", 0),
    p95=search_report.get("p95_us", 0),
    p99=search_report.get("p99_us", 0),
    max_us=search_report.get("max_us", 0),
    budget_default=5_000,
    source_path=search_path,
    extra={"verdict": "PASS" if search_report.get("passed", False) else "FAIL"},
)

db_thresholds = db_report.get("thresholds", {})
if db_report.get("verdict", "").startswith("FAIL") or not all(
    bool(v) for k, v in db_thresholds.items() if k.endswith("_pass")
):
    failures.append("db_sustained_session: threshold report failed")
if tui_report.get("verdict") != "PASS":
    failures.append("tui_render_loop: soak_replay_empty_state verdict is not PASS")
if not rapid_report.get("passed", False):
    failures.append("tui_interaction_rapid_screen_cycle: report verdict is FAIL")
if not search_report.get("passed", False):
    failures.append("tui_search_typing: report verdict is FAIL")

trend_path_obj = Path(trend_path)
trend_path_obj.parent.mkdir(parents=True, exist_ok=True)
with trend_path_obj.open("a", encoding="utf-8") as f:
    for row in rows:
        f.write(json.dumps(row, sort_keys=True) + "\n")

summary = {
    "generated_at": generated_at,
    "profile": profile,
    "seed": seed,
    "rows": rows,
    "failures": failures,
}
Path(summary_path).write_text(json.dumps(summary, indent=2, sort_keys=True), encoding="utf-8")

if failures:
    print("Perf trend synthesis failures:")
    for item in failures:
        print(f"  - {item}")
    sys.exit(1)

print(f"Perf trend artifact: {trend_path}")
print(f"Perf trend summary: {summary_path}")
PY

    echo "Perf trend artifact: $trend_file"
    echo "Perf summary artifact: $summary_file"
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_copy_artifact "$trend_dir" "metrics/soak_perf_trends"
    fi
}

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

# ── Phase 7: TUI Search Typing Stress ──
if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_case_banner "tui search typing stress"
else
    echo "--- Phase 7: TUI search typing stress ---"
fi
if cargo test -p mcp-agent-mail-server --test tui_soak_replay soak_search_typing_stress -- --nocapture 2>&1; then
    echo "PASS: soak_search_typing_stress"
    PASS=$((PASS + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_pass "soak_search_typing_stress"
    fi
else
    echo "FAIL: soak_search_typing_stress"
    FAIL=$((FAIL + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_fail "soak_search_typing_stress"
    fi
fi
echo ""

# ── Phase 8: Perf Trend Synthesis + Baseline Gates ──
if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
    e2e_case_banner "perf trend synthesis"
else
    echo "--- Phase 8: Perf trend synthesis ---"
fi
if emit_soak_perf_trends; then
    echo "PASS: soak_perf_trends"
    PASS=$((PASS + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_pass "soak_perf_trends"
    fi
else
    echo "FAIL: soak_perf_trends"
    FAIL=$((FAIL + 1))
    if [[ "$HAS_E2E_LIB" -eq 1 ]]; then
        e2e_fail "soak_perf_trends"
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
