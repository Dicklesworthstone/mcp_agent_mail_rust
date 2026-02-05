#!/usr/bin/env bash
# bench_cli.sh - CLI performance benchmarks using hyperfine.
#
# Usage:
#   ./scripts/bench_cli.sh              # Run all benchmarks
#   ./scripts/bench_cli.sh --json       # Output JSON summary
#   ./scripts/bench_cli.sh --quick      # Quick mode (fewer runs)
#
# Requires: hyperfine
# Outputs: benches/results/cli_<timestamp>.json

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
# Aggregate results
# ---------------------------------------------------------------------------
log "Aggregating results..."
python3 -c "
import json, glob, os, sys

results_dir = sys.argv[1]
timestamp = sys.argv[2]

summary = {
    'timestamp': timestamp,
    'hardware': {
        'hostname': os.uname().nodename,
        'arch': os.uname().machine,
        'kernel': os.uname().release,
    },
    'benchmarks': {}
}

for f in sorted(glob.glob(os.path.join(results_dir, f'*_{timestamp}.json'))):
    name = os.path.basename(f).replace(f'_{timestamp}.json', '')
    try:
        data = json.load(open(f))
        if 'results' in data and data['results']:
            r = data['results'][0]
            summary['benchmarks'][name] = {
                'mean_ms': round(r['mean'] * 1000, 2),
                'stddev_ms': round(r['stddev'] * 1000, 2),
                'min_ms': round(r['min'] * 1000, 2),
                'max_ms': round(r['max'] * 1000, 2),
                'median_ms': round(r['median'] * 1000, 2),
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
