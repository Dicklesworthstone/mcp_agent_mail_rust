#!/usr/bin/env bash
# ci.sh - Local CI runner: runs the same suite as .github/workflows/ci.yml
#
# Usage:
#   bash scripts/ci.sh          # Run all gates
#   bash scripts/ci.sh --quick  # Skip E2E (faster)
#   bash scripts/ci.sh --report tests/artifacts/ci/custom_report.json
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
REPORT_PATH="${CI_GATE_REPORT_PATH:-tests/artifacts/ci/gate_report.json}"
GATE_LOG_PATH=""

usage() {
    cat <<'EOF'
Usage: bash scripts/ci.sh [--quick] [--report <path>]

Options:
  --quick          Skip long-running E2E gates.
  --report <path>  Write machine-readable gate report JSON to <path>.
                   Quick runs always emit decision=no-go.
  -h, --help       Show this help text.
EOF
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --quick)
            QUICK=1
            shift
            ;;
        --report)
            if [ "$#" -lt 2 ]; then
                echo "error: --report requires a path argument" >&2
                exit 2
            fi
            REPORT_PATH="$2"
            shift 2
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            echo "error: unknown argument: $1" >&2
            usage >&2
            exit 2
            ;;
    esac
done

REPORT_DIR="$(dirname "$REPORT_PATH")"
mkdir -p "$REPORT_DIR"
GATE_LOG_PATH="${REPORT_PATH%.json}.gates.ndjson"
: > "$GATE_LOG_PATH"

PASS=0
FAIL=0
SKIP=0

gate() {
    local category="$1"
    shift
    local name="$1"
    shift
    local cmd_display="$*"
    echo ""
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    echo "  GATE [$category]: $name"
    echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
    local start_time
    start_time=$(date +%s)
    local elapsed=0
    local status="fail"
    if "$@"; then
        elapsed=$(( $(date +%s) - start_time ))
        echo "  PASS: $name (${elapsed}s)"
        PASS=$((PASS + 1))
        status="pass"
    else
        elapsed=$(( $(date +%s) - start_time ))
        echo "  FAIL: $name (${elapsed}s)"
        FAIL=$((FAIL + 1))
        status="fail"
    fi
    jq -cn \
        --arg category "$category" \
        --arg name "$name" \
        --arg status "$status" \
        --arg command "$cmd_display" \
        --argjson elapsed_seconds "$elapsed" \
        '{
            category: $category,
            name: $name,
            status: $status,
            elapsed_seconds: $elapsed_seconds,
            command: $command
        }' >> "$GATE_LOG_PATH"
}

skip_gate() {
    local category="$1"
    shift
    local name="$1"
    shift
    local reason="${1:-skip}"
    echo "  SKIP [$category]: $name"
    SKIP=$((SKIP + 1))
    jq -cn \
        --arg category "$category" \
        --arg name "$name" \
        --arg status "skip" \
        --arg command "$reason" \
        --argjson elapsed_seconds 0 \
        '{
            category: $category,
            name: $name,
            status: $status,
            elapsed_seconds: $elapsed_seconds,
            command: $command
        }' >> "$GATE_LOG_PATH"
}

# ── Gates ─────────────────────────────────────────────────────────────

gate quality "Format check" cargo fmt --all -- --check

gate quality "Clippy" cargo clippy --workspace --all-targets -- -D warnings

gate quality "Build workspace" cargo build --workspace

gate quality "Unit + integration tests" cargo test --workspace

gate quality "Mode matrix harness" cargo test -p mcp-agent-mail-cli --test mode_matrix_harness -- --nocapture

gate quality "Semantic conformance" cargo test -p mcp-agent-mail-cli --test semantic_conformance -- --nocapture

gate performance "Perf + security regressions" cargo test -p mcp-agent-mail-cli --test perf_security_regressions -- --nocapture

gate quality "Help snapshots" cargo test -p mcp-agent-mail-cli --test help_snapshots -- --nocapture

gate docs "Release docs references present" bash -c 'test -f docs/RELEASE_CHECKLIST.md && test -f docs/ROLLOUT_PLAYBOOK.md && test -f docs/OPERATOR_RUNBOOK.md'

if [ "$QUICK" -eq 0 ]; then
    gate quality "E2E dual-mode" bash scripts/e2e_dual_mode.sh
    gate quality "E2E mode matrix" bash scripts/e2e_mode_matrix.sh
    gate security "E2E security/privacy" bash tests/e2e/test_security_privacy.sh
    gate quality "E2E TUI accessibility" bash scripts/e2e_tui_a11y.sh
else
    skip_gate quality "E2E dual-mode" "--quick"
    skip_gate quality "E2E mode matrix" "--quick"
    skip_gate security "E2E security/privacy" "--quick"
    skip_gate quality "E2E TUI accessibility" "--quick"
fi

# ── Summary ───────────────────────────────────────────────────────────

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "  CI Summary: Pass=$PASS  Fail=$FAIL  Skip=$SKIP"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

RUN_MODE="full"
if [ "$QUICK" -eq 1 ]; then
    RUN_MODE="quick"
fi

RELEASE_ELIGIBLE=false
DECISION="no-go"
DECISION_REASON="quick mode skips required release gates"
if [ "$FAIL" -gt 0 ]; then
    DECISION="no-go"
    DECISION_REASON="one or more gates failed"
elif [ "$RUN_MODE" = "full" ]; then
    DECISION="go"
    DECISION_REASON="all required full-run gates passed"
    RELEASE_ELIGIBLE=true
fi

GENERATED_AT="$(date -u +"%Y-%m-%dT%H:%M:%SZ")"

jq -cs \
    --arg schema_version "am_ci_gate_report.v1" \
    --arg generated_at "$GENERATED_AT" \
    --arg mode "$RUN_MODE" \
    --arg decision "$DECISION" \
    --arg decision_reason "$DECISION_REASON" \
    --argjson release_eligible "$RELEASE_ELIGIBLE" \
    --arg checklist_reference "docs/RELEASE_CHECKLIST.md" \
    '
    def required_count(cat): map(select(.category == cat and .status != "skip")) | length;
    def pass_count(cat): map(select(.category == cat and .status == "pass")) | length;
    def fail_count(cat): map(select(.category == cat and .status == "fail")) | length;
    def pass_rate(cat):
        (required_count(cat)) as $required
        | if $required == 0 then null else ((pass_count(cat)) / $required) end;
    def gate_status(name):
        (map(select(.name == name)) | if length == 0 then "missing" else .[-1].status end);
    . as $gates
    | {
        schema_version: $schema_version,
        generated_at: $generated_at,
        mode: $mode,
        decision: $decision,
        decision_reason: $decision_reason,
        release_eligible: $release_eligible,
        checklist_reference: $checklist_reference,
        summary: {
            total: ($gates | length),
            pass: ($gates | map(select(.status == "pass")) | length),
            fail: ($gates | map(select(.status == "fail")) | length),
            skip: ($gates | map(select(.status == "skip")) | length)
        },
        thresholds: {
            quality: {
                required_pass_rate: 1,
                observed_pass_rate: pass_rate("quality"),
                required_gates: required_count("quality"),
                failed_gates: fail_count("quality")
            },
            performance: {
                required_pass_rate: 1,
                observed_pass_rate: pass_rate("performance"),
                required_gates: required_count("performance"),
                failed_gates: fail_count("performance")
            },
            security: {
                required_pass_rate: 1,
                observed_pass_rate: pass_rate("security"),
                required_gates: required_count("security"),
                failed_gates: fail_count("security")
            },
            docs: {
                required_pass_rate: 1,
                observed_pass_rate: pass_rate("docs"),
                required_gates: required_count("docs"),
                failed_gates: fail_count("docs")
            }
        },
        gate_logic: {
            security_privacy_gate: {
                gate: "E2E security/privacy",
                status: gate_status("E2E security/privacy"),
                threshold: "must pass (non-quick runs)"
            },
            accessibility_gate: {
                gate: "E2E TUI accessibility",
                status: gate_status("E2E TUI accessibility"),
                threshold: "must pass (non-quick runs)"
            },
            performance_gate: {
                gate: "Perf + security regressions",
                status: gate_status("Perf + security regressions"),
                threshold: "must pass"
            },
            go_condition: "all non-skipped gates pass"
        },
        gates: $gates
    }' "$GATE_LOG_PATH" > "$REPORT_PATH"

echo "  Gate report: $REPORT_PATH"
echo "  Gate log: $GATE_LOG_PATH"

if [ "$DECISION" = "no-go" ]; then
    echo "  RESULT: FAILED"
    exit 1
fi
echo "  RESULT: PASSED"
exit 0
