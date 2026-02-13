#!/usr/bin/env bash
# DEPRECATED: Native `am bench` is authoritative for benchmark workflows.
# bench_cli.sh - compatibility shim for legacy benchmark invocations.
#
# Usage:
#   ./scripts/bench_cli.sh              # Deprecated: use `am bench`
#   ./scripts/bench_cli.sh --json       # Deprecated: use `am bench --json`
#   ./scripts/bench_cli.sh --quick      # Deprecated: use `am bench --quick`
#
# Requires: hyperfine
# Outputs: benches/results/cli_<timestamp>.json
#
# Command mapping:
#   ./scripts/bench_cli.sh              -> am bench
#   ./scripts/bench_cli.sh --quick      -> am bench --quick
#   ./scripts/bench_cli.sh --json       -> am bench --json

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
RESULTS_DIR="${PROJECT_ROOT}/benches/results"
CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"
AM="${CARGO_TARGET_DIR}/debug/am"
TIMESTAMP="$(date -u '+%Y%m%d_%H%M%S')"
JSON_OUT="${RESULTS_DIR}/cli_${TIMESTAMP}.json"

# Colors
_reset='\033[0m'
_blue='\033[0;34m'
_green='\033[0;32m'

log() { echo -e "${_blue}[bench]${_reset} $*"; }

# Parse args
JSON_MODE=false
WARMUP=3
RUNS=10
for arg in "$@"; do
    case "$arg" in
        --json) JSON_MODE=true ;;
        --quick) WARMUP=1; RUNS=3 ;;
    esac
done

echo "WARNING: scripts/bench_cli.sh is deprecated. Use 'am bench' instead." >&2
echo "         Artifacts: benches/results/*.json (legacy) and am bench reports (native)." >&2

# Check prerequisites
if ! command -v hyperfine &>/dev/null; then
    echo "ERROR: hyperfine not found. Install via: cargo install hyperfine"
    exit 1
fi

# Build release binary for realistic benchmarks
log "Building release binary..."
cargo build -p mcp-agent-mail-cli --release 2>&1 | tail -3
AM_RELEASE="${CARGO_TARGET_DIR}/release/am"
if [ ! -f "$AM_RELEASE" ]; then
    log "Release binary not available, using debug"
    AM_RELEASE="$AM"
fi

mkdir -p "$RESULTS_DIR"

log "Benchmarking with: ${AM_RELEASE}"
log "Warmup: ${WARMUP}, Runs: ${RUNS}"
log ""

# Create temp workspace for benchmarks
WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# ---------------------------------------------------------------------------
# Benchmark 1: am --help (startup time baseline)
# ---------------------------------------------------------------------------
log "Benchmark: am --help (startup time)"
hyperfine \
    --warmup "$WARMUP" \
    --runs "$RUNS" \
    --export-json "${RESULTS_DIR}/help_${TIMESTAMP}.json" \
    "${AM_RELEASE} --help"

# ---------------------------------------------------------------------------
# Benchmark 2: am lint (analysis tool)
# ---------------------------------------------------------------------------
log "Benchmark: am lint"
export MCP_AGENT_MAIL_DB="${WORK}/bench.db"
hyperfine \
    --warmup "$WARMUP" \
    --runs "$RUNS" \
    --export-json "${RESULTS_DIR}/lint_${TIMESTAMP}.json" \
    "${AM_RELEASE} lint" 2>/dev/null || log "lint benchmark skipped (not ready)"

# ---------------------------------------------------------------------------
# Benchmark 3: am typecheck
# ---------------------------------------------------------------------------
log "Benchmark: am typecheck"
hyperfine \
    --warmup "$WARMUP" \
    --runs "$RUNS" \
    --export-json "${RESULTS_DIR}/typecheck_${TIMESTAMP}.json" \
    "${AM_RELEASE} typecheck" 2>/dev/null || log "typecheck benchmark skipped (not ready)"

# ---------------------------------------------------------------------------
# Benchmark 4: Stub encoder throughput
# ---------------------------------------------------------------------------
STUB="${PROJECT_ROOT}/scripts/toon_stub_encoder.sh"
if [ -x "$STUB" ]; then
    log "Benchmark: stub encoder throughput"

    # Small payload
    echo '{"id":1}' > "${WORK}/small.json"
    hyperfine \
        --warmup "$WARMUP" \
        --runs "$RUNS" \
        --export-json "${RESULTS_DIR}/stub_small_${TIMESTAMP}.json" \
        "${STUB} --encode < ${WORK}/small.json"

    # Medium payload (~500 bytes)
    python3 -c "
import json
d = {'id': 1, 'messages': [{'id': i, 'subject': f'msg_{i}', 'body': 'x'*100} for i in range(5)]}
print(json.dumps(d))
" > "${WORK}/medium.json"
    hyperfine \
        --warmup "$WARMUP" \
        --runs "$RUNS" \
        --export-json "${RESULTS_DIR}/stub_medium_${TIMESTAMP}.json" \
        "${STUB} --encode < ${WORK}/medium.json"

    # With stats
    hyperfine \
        --warmup "$WARMUP" \
        --runs "$RUNS" \
        --export-json "${RESULTS_DIR}/stub_stats_${TIMESTAMP}.json" \
        "${STUB} --encode --stats < ${WORK}/small.json"
fi

# ---------------------------------------------------------------------------
# Benchmark 5: Operational commands (require seeded DB)
# ---------------------------------------------------------------------------
log ""
log "=== Operational Benchmarks (seeded DB) ==="

BENCH_DB="${WORK}/bench.db"
BENCH_ARCHIVE="${WORK}/archive"
mkdir -p "${BENCH_ARCHIVE}"
export MCP_AGENT_MAIL_DB="${BENCH_DB}"
export MCP_AGENT_MAIL_ARCHIVE_ROOT="${BENCH_ARCHIVE}"

# Seed the DB with a project, 2 agents, and 50 messages
log "Seeding benchmark database..."
SEED_OK=true

# Register project and agents first
"${AM_RELEASE}" agents register \
    --project /tmp/bench --name BlueLake \
    --program bench --model bench --json >/dev/null 2>&1 || SEED_OK=false
"${AM_RELEASE}" agents register \
    --project /tmp/bench --name RedFox \
    --program bench --model bench --json >/dev/null 2>&1 || SEED_OK=false

# Send first message
if [ "$SEED_OK" = true ]; then
    "${AM_RELEASE}" mail send \
        --project /tmp/bench --from BlueLake --to RedFox \
        --subject "seed-0" --body "initial seed" --json >/dev/null 2>&1 || SEED_OK=false
fi

if [ "$SEED_OK" = true ]; then
    # Send 49 more messages to create a realistic inbox
    for i in $(seq 1 49); do
        "${AM_RELEASE}" mail send \
            --project /tmp/bench --from BlueLake --to RedFox \
            --subject "bench message $i" --body "body of message $i for benchmarking" \
            --json >/dev/null 2>&1 || true
    done

    # Send some in the other direction for outbox variety
    for i in $(seq 1 10); do
        "${AM_RELEASE}" mail send \
            --project /tmp/bench --from RedFox --to BlueLake \
            --subject "reply $i" --body "reply body $i" \
            --json >/dev/null 2>&1 || true
    done

    log "Seeded: 60 messages (50 + 10 replies)"

    # 5a: mail inbox (cold - first read from DB)
    log "Benchmark: mail inbox (50 messages)"
    hyperfine \
        --warmup "$WARMUP" \
        --runs "$RUNS" \
        --export-json "${RESULTS_DIR}/mail_inbox_${TIMESTAMP}.json" \
        "${AM_RELEASE} mail inbox --project /tmp/bench --agent RedFox --json"

    # 5b: mail inbox with bodies
    log "Benchmark: mail inbox --include-bodies"
    hyperfine \
        --warmup "$WARMUP" \
        --runs "$RUNS" \
        --export-json "${RESULTS_DIR}/mail_inbox_bodies_${TIMESTAMP}.json" \
        "${AM_RELEASE} mail inbox --project /tmp/bench --agent RedFox --include-bodies --json"

    # 5c: mail send (single message - measures full write path)
    log "Benchmark: mail send (single message)"
    hyperfine \
        --warmup "$WARMUP" \
        --runs "$RUNS" \
        --export-json "${RESULTS_DIR}/mail_send_${TIMESTAMP}.json" \
        "${AM_RELEASE} mail send --project /tmp/bench --from BlueLake --to RedFox --subject bench-msg --body bench-body --json"

    # 5d: mail search (FTS5 query)
    log "Benchmark: mail search"
    hyperfine \
        --warmup "$WARMUP" \
        --runs "$RUNS" \
        --export-json "${RESULTS_DIR}/mail_search_${TIMESTAMP}.json" \
        "${AM_RELEASE} mail search --project /tmp/bench --json bench" 2>/dev/null \
        || log "mail search benchmark skipped"

    # 5e: doctor check
    log "Benchmark: doctor check"
    hyperfine \
        --warmup "$WARMUP" \
        --runs "$RUNS" \
        --export-json "${RESULTS_DIR}/doctor_check_${TIMESTAMP}.json" \
        "${AM_RELEASE} doctor check --json" 2>/dev/null \
        || log "doctor check benchmark skipped"

    # 5f: list-projects
    log "Benchmark: list-projects"
    hyperfine \
        --warmup "$WARMUP" \
        --runs "$RUNS" \
        --export-json "${RESULTS_DIR}/list_projects_${TIMESTAMP}.json" \
        "${AM_RELEASE} list-projects --json" 2>/dev/null \
        || log "list-projects benchmark skipped"

    # 5g: agents list
    log "Benchmark: agents list"
    hyperfine \
        --warmup "$WARMUP" \
        --runs "$RUNS" \
        --export-json "${RESULTS_DIR}/agents_list_${TIMESTAMP}.json" \
        "${AM_RELEASE} agents list --project /tmp/bench --json" 2>/dev/null \
        || log "agents list benchmark skipped"
else
    log "WARNING: Could not seed DB; operational benchmarks skipped"
fi

# ---------------------------------------------------------------------------
# Aggregate results
# ---------------------------------------------------------------------------
log "Aggregating results..."
python3 -c "
import glob
import hashlib
import json
import os
import sys

results_dir = sys.argv[1]
timestamp = sys.argv[2]

def pct(values, p):
    if not values:
        return 0.0
    idx = int(round((p / 100.0) * (len(values) - 1)))
    idx = max(0, min(idx, len(values) - 1))
    return values[idx]

baseline_path = os.getenv('BENCH_BASELINE_FILE', '')
baselines = {}
if baseline_path and os.path.isfile(baseline_path):
    try:
        with open(baseline_path) as bf:
            baselines = json.load(bf)
    except Exception:
        baselines = {}

summary = {
    'timestamp': timestamp,
    'schema_version': 1,
    'hardware': {
        'hostname': os.uname().nodename,
        'arch': os.uname().machine,
        'kernel': os.uname().release,
    },
    'environment_profile': {
        'python': sys.version.split()[0],
        'cwd': os.getcwd(),
    },
    'benchmarks': {}
}

for f in sorted(glob.glob(os.path.join(results_dir, f'*_{timestamp}.json'))):
    name = os.path.basename(f).replace(f'_{timestamp}.json', '')
    try:
        data = json.load(open(f))
        if 'results' in data and data['results']:
            r = data['results'][0]
            samples_ms = sorted([round(v * 1000, 4) for v in r.get('times', [])])
            p95_ms = round(pct(samples_ms, 95.0), 2)
            p99_ms = round(pct(samples_ms, 99.0), 2)
            variance_ms2 = round((r.get('stddev', 0.0) * 1000) ** 2, 4)
            baseline_p95_ms = None
            delta_p95_ms = None
            if isinstance(baselines, dict):
                baseline = baselines.get(name, {})
                if isinstance(baseline, dict):
                    baseline_p95_ms = baseline.get('p95_ms')
                elif isinstance(baseline, (int, float)):
                    baseline_p95_ms = float(baseline)
                if baseline_p95_ms is not None:
                    delta_p95_ms = round(p95_ms - float(baseline_p95_ms), 2)

            fixture_material = '|'.join([
                name,
                r.get('command', ''),
                str(r.get('parameters', {})),
                os.uname().machine,
                os.uname().release,
            ])
            fixture_signature = hashlib.sha256(fixture_material.encode('utf-8')).hexdigest()[:16]

            summary['benchmarks'][name] = {
                'mean_ms': round(r['mean'] * 1000, 2),
                'stddev_ms': round(r['stddev'] * 1000, 2),
                'variance_ms2': variance_ms2,
                'min_ms': round(r['min'] * 1000, 2),
                'max_ms': round(r['max'] * 1000, 2),
                'median_ms': round(r['median'] * 1000, 2),
                'p95_ms': p95_ms,
                'p99_ms': p99_ms,
                'baseline_p95_ms': baseline_p95_ms,
                'delta_p95_ms': delta_p95_ms,
                'timeseries_ms': samples_ms,
                'fixture_signature': fixture_signature,
                'command': r.get('command', ''),
            }
    except Exception as e:
        print(f'Warning: {f}: {e}', file=sys.stderr)

out_path = os.path.join(results_dir, f'summary_{timestamp}.json')
with open(out_path, 'w') as f:
    json.dump(summary, f, indent=2)
print(json.dumps(summary, indent=2))
" "$RESULTS_DIR" "$TIMESTAMP"

log ""
log "Results directory: ${RESULTS_DIR}"
log "Summary: ${RESULTS_DIR}/summary_${TIMESTAMP}.json"
