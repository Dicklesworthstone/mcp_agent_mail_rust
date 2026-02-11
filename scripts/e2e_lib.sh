#!/usr/bin/env bash
# e2e_lib.sh - Shared helpers for mcp-agent-mail E2E test suites
# Source this file from individual test scripts.
#
# Provides:
#   - Temp workspace creation + cleanup
#   - Artifact directory management
#   - Structured logging (banners, pass/fail, expected vs actual)
#   - File tree dumps and stable hashing
#   - Retry helpers for flaky port binds
#   - Environment dump (secrets redacted)

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

# Suite name: set by each test script before sourcing
E2E_SUITE="${E2E_SUITE:-unknown}"

# Keep temp dirs on failure for debugging
AM_E2E_KEEP_TMP="${AM_E2E_KEEP_TMP:-0}"

# Prefer a large temp root when available (some environments run out of /tmp tmpfs).
# Honor an explicit TMPDIR if the caller provided one.
if [ -z "${TMPDIR:-}" ]; then
    if [ -d "/data/tmp" ]; then
        export TMPDIR="/data/tmp"
    else
        export TMPDIR="/tmp"
    fi
fi

# Cargo target dir: avoid multi-agent contention
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="/data/tmp/cargo-target"
fi

# Root of the project
E2E_PROJECT_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# ---------------------------------------------------------------------------
# Determinism / replay controls (br-3vwi.10.19)
# ---------------------------------------------------------------------------
#
# Use these env vars to make runs replayable with stable timestamps and harness-
# generated IDs:
#   E2E_CLOCK_MODE=wall|deterministic
#   E2E_SEED=<u64>
#   E2E_TIMESTAMP=<YYYYmmdd_HHMMSS>
#   E2E_RUN_STARTED_AT=<rfc3339>
#   E2E_RUN_START_EPOCH_S=<epoch seconds>
#
E2E_CLOCK_MODE="${E2E_CLOCK_MODE:-wall}"
E2E_CLOCK_MODE="$(printf '%s' "$E2E_CLOCK_MODE" | tr '[:upper:]' '[:lower:]')"

E2E_SEED="${E2E_SEED:-}"
E2E_TIMESTAMP="${E2E_TIMESTAMP:-}"
E2E_RUN_STARTED_AT="${E2E_RUN_STARTED_AT:-}"
E2E_RUN_START_EPOCH_S="${E2E_RUN_START_EPOCH_S:-}"

# Artifact run timestamp is always wall-clock by default (avoids clobbering
# prior artifact dirs when replaying deterministic runs).
if [ -z "$E2E_TIMESTAMP" ]; then
    E2E_TIMESTAMP="$(date -u '+%Y%m%d_%H%M%S')"
fi

if [ -z "$E2E_SEED" ]; then
    # Default seed: numeric form of the UTC run timestamp.
    E2E_SEED="${E2E_TIMESTAMP//_/}"
fi

if [ "$E2E_CLOCK_MODE" = "deterministic" ]; then
    # Derive logical time from the seed unless explicitly pinned.
    if [ -z "$E2E_RUN_START_EPOCH_S" ]; then
        # Stable epoch derived from seed (mod 1 day). Base epoch is arbitrary but fixed.
        E2E_RUN_START_EPOCH_S=$(( 1700000000 + (E2E_SEED % 86400) ))
    fi

    if [ -z "$E2E_RUN_STARTED_AT" ]; then
        E2E_RUN_STARTED_AT="$(date -u -d "@${E2E_RUN_START_EPOCH_S}" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date -u '+%Y-%m-%dT%H:%M:%SZ')"
    fi
else
    if [ -z "$E2E_RUN_STARTED_AT" ]; then
        E2E_RUN_STARTED_AT="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    fi

    if [ -z "$E2E_RUN_START_EPOCH_S" ]; then
        E2E_RUN_START_EPOCH_S="$(date +%s)"
    fi
fi

# Artifact directory for this run
E2E_ARTIFACT_DIR="${E2E_PROJECT_ROOT}/tests/artifacts/${E2E_SUITE}/${E2E_TIMESTAMP}"

# Run timing (used for artifact bundle metadata/metrics)
E2E_RUN_ENDED_AT=""
E2E_RUN_END_EPOCH_S="0"

# Deterministic trace clock: monotonically incremented seconds since E2E_RUN_START_EPOCH_S.
_E2E_TRACE_SEQ=0

# Deterministic RNG state (used only by harness helpers; suites may opt-in).
_E2E_RNG_STATE=0

# Counters
_E2E_PASS=0
_E2E_FAIL=0
_E2E_SKIP=0
_E2E_TOTAL=0

# Current case (for trace correlation)
_E2E_CURRENT_CASE=""

# Trace file (initialized by e2e_init_artifacts)
_E2E_TRACE_FILE=""

# Temp dirs to clean up
_E2E_TMP_DIRS=()

# ---------------------------------------------------------------------------
# Deterministic helpers (br-3vwi.10.19)
# ---------------------------------------------------------------------------

_e2e_rng_init() {
    # Small bash-native RNG for stable IDs (NOT cryptographic).
    if [[ "${E2E_SEED}" =~ ^[0-9]+$ ]]; then
        _E2E_RNG_STATE=$(( E2E_SEED & 0x7fffffff ))
    else
        _E2E_RNG_STATE=0
    fi
}

_e2e_rng_next_u32() {
    # Deterministic LCG (glibc-ish constants), masked to 31 bits.
    _E2E_RNG_STATE=$(( (1103515245 * _E2E_RNG_STATE + 12345) & 0x7fffffff ))
    echo "$_E2E_RNG_STATE"
}

e2e_seeded_hex() {
    local n
    n="$(_e2e_rng_next_u32)"
    printf '%08x' "$n"
}

e2e_seeded_id() {
    local prefix="${1:-id}"
    echo "${prefix}_$(e2e_seeded_hex)"
}

e2e_repro_command() {
    # Copy/paste friendly one-liner for deterministic replay.
    # Note: We intentionally do NOT pin E2E_TIMESTAMP so each replay writes to a fresh artifact dir.
    local suite="${E2E_SUITE}"
    printf 'cd %q && AM_E2E_KEEP_TMP=1 E2E_CLOCK_MODE=%q E2E_SEED=%q E2E_RUN_STARTED_AT=%q E2E_RUN_START_EPOCH_S=%q ./scripts/e2e_test.sh %q\n' \
        "$E2E_PROJECT_ROOT" \
        "${E2E_CLOCK_MODE}" \
        "${E2E_SEED}" \
        "${E2E_RUN_STARTED_AT}" \
        "${E2E_RUN_START_EPOCH_S}" \
        "${suite}"
}

_e2e_rng_init

# ---------------------------------------------------------------------------
# Logging
# ---------------------------------------------------------------------------

_e2e_color_reset='\033[0m'
_e2e_color_green='\033[0;32m'
_e2e_color_red='\033[0;31m'
_e2e_color_yellow='\033[0;33m'
_e2e_color_blue='\033[0;34m'
_e2e_color_dim='\033[0;90m'

e2e_log() {
    echo -e "${_e2e_color_dim}[e2e]${_e2e_color_reset} $*" >&2
}

e2e_banner() {
    local msg="$1"
    echo ""
    echo -e "${_e2e_color_blue}════════════════════════════════════════════════════════════${_e2e_color_reset}"
    echo -e "${_e2e_color_blue}  ${msg}${_e2e_color_reset}"
    echo -e "${_e2e_color_blue}════════════════════════════════════════════════════════════${_e2e_color_reset}"
}

e2e_case_banner() {
    local case_name="$1"
    (( _E2E_TOTAL++ )) || true
    _E2E_CURRENT_CASE="$case_name"
    _e2e_trace_event "case_start" "" "$case_name"
    echo ""
    echo -e "${_e2e_color_blue}── Case: ${case_name} ──${_e2e_color_reset}"
}

e2e_pass() {
    local msg="${1:-}"
    (( _E2E_PASS++ )) || true
    _e2e_trace_event "assert_pass" "$msg"
    echo -e "  ${_e2e_color_green}PASS${_e2e_color_reset} ${msg}"
}

e2e_fail() {
    local msg="${1:-}"
    (( _E2E_FAIL++ )) || true
    _e2e_trace_event "assert_fail" "$msg"
    echo -e "  ${_e2e_color_red}FAIL${_e2e_color_reset} ${msg}"
}

e2e_skip() {
    local msg="${1:-}"
    (( _E2E_SKIP++ )) || true
    _e2e_trace_event "assert_skip" "$msg"
    echo -e "  ${_e2e_color_yellow}SKIP${_e2e_color_reset} ${msg}"
}

# Print expected vs actual for a mismatch
e2e_diff() {
    local label="$1"
    local expected="$2"
    local actual="$3"
    echo -e "  ${_e2e_color_red}MISMATCH${_e2e_color_reset} ${label}"
    echo -e "    expected: ${_e2e_color_green}${expected}${_e2e_color_reset}"
    echo -e "    actual:   ${_e2e_color_red}${actual}${_e2e_color_reset}"
}

# Assert two strings are equal
e2e_assert_eq() {
    local label="$1"
    local expected="$2"
    local actual="$3"
    if [ "$expected" = "$actual" ]; then
        e2e_pass "$label"
    else
        e2e_fail "$label"
        e2e_diff "$label" "$expected" "$actual"
    fi
}

# Assert a string contains a substring
e2e_assert_contains() {
    local label="$1"
    local haystack="$2"
    local needle="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        e2e_pass "$label"
    else
        e2e_fail "$label"
        echo -e "    expected to contain: ${_e2e_color_green}${needle}${_e2e_color_reset}"
        echo -e "    in: ${_e2e_color_red}${haystack}${_e2e_color_reset}"
    fi
}

# Assert a string does NOT contain a substring
e2e_assert_not_contains() {
    local label="$1"
    local haystack="$2"
    local needle="$3"
    if [[ "$haystack" == *"$needle"* ]]; then
        e2e_fail "$label"
        echo -e "    expected to NOT contain: ${_e2e_color_green}${needle}${_e2e_color_reset}"
    else
        e2e_pass "$label"
    fi
}

# Assert a file exists
e2e_assert_file_exists() {
    local label="$1"
    local path="$2"
    if [ -f "$path" ]; then
        e2e_pass "$label"
    else
        e2e_fail "$label: file not found: $path"
    fi
}

# Assert a directory exists
e2e_assert_dir_exists() {
    local label="$1"
    local path="$2"
    if [ -d "$path" ]; then
        e2e_pass "$label"
    else
        e2e_fail "$label: directory not found: $path"
    fi
}

# Assert exit code
e2e_assert_exit_code() {
    local label="$1"
    local expected="$2"
    local actual="$3"
    if [ "$expected" = "$actual" ]; then
        e2e_pass "$label (exit=$actual)"
    else
        e2e_fail "$label"
        e2e_diff "exit code" "$expected" "$actual"
    fi
}

# ---------------------------------------------------------------------------
# Temp workspace management
# ---------------------------------------------------------------------------

# Create a temp directory and register it for cleanup
e2e_mktemp() {
    local prefix="${1:-e2e}"
    local td
    td="$(mktemp -d "${TMPDIR%/}/${prefix}.XXXXXX")"
    _E2E_TMP_DIRS+=("$td")
    echo "$td"
}

# Cleanup function: remove temp dirs unless AM_E2E_KEEP_TMP=1
_e2e_cleanup() {
    if [ "$AM_E2E_KEEP_TMP" = "1" ] || [ "$AM_E2E_KEEP_TMP" = "true" ]; then
        if [ ${#_E2E_TMP_DIRS[@]} -gt 0 ]; then
            e2e_log "Keeping temp dirs (AM_E2E_KEEP_TMP=1):"
            for d in "${_E2E_TMP_DIRS[@]}"; do
                e2e_log "  $d"
            done
        fi
        return
    fi
    for d in "${_E2E_TMP_DIRS[@]}"; do
        rm -rf "$d" 2>/dev/null || true
    done
}

trap _e2e_cleanup EXIT

# ---------------------------------------------------------------------------
# Artifact management
# ---------------------------------------------------------------------------

# Initialize the artifact directory for this run
e2e_init_artifacts() {
    mkdir -p "$E2E_ARTIFACT_DIR"/{diagnostics,trace,transcript}
    _E2E_TRACE_FILE="${E2E_ARTIFACT_DIR}/trace/events.jsonl"
    touch "$_E2E_TRACE_FILE"
    _e2e_trace_event "suite_start" ""
    e2e_log "Artifacts: $E2E_ARTIFACT_DIR"
}

# Save a file to the artifact directory
e2e_save_artifact() {
    local name="$1"
    local content="$2"
    local dest="${E2E_ARTIFACT_DIR}/${name}"
    mkdir -p "$(dirname "$dest")"
    echo "$content" > "$dest"
}

# Save a file (by path) to artifacts
e2e_copy_artifact() {
    local src="$1"
    local dest_name="${2:-$(basename "$src")}"
    local dest="${E2E_ARTIFACT_DIR}/${dest_name}"
    mkdir -p "$(dirname "$dest")"
    cp -r "$src" "$dest" 2>/dev/null || true
}

# ---------------------------------------------------------------------------
# Artifact bundle schema (br-3vwi.10.18)
# ---------------------------------------------------------------------------

_e2e_json_escape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/\\r}"
    s="${s//$'\t'/\\t}"
    echo -n "$s"
}

_e2e_stat_bytes() {
    local file="$1"
    stat --format='%s' "$file" 2>/dev/null || stat -f '%z' "$file" 2>/dev/null || echo "0"
}

e2e_write_repro_files() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    if [ ! -d "$artifact_dir" ]; then
        return 0
    fi

    local cmd
    cmd="$(e2e_repro_command)"

    cat > "${artifact_dir}/repro.txt" <<EOF
Repro (br-3vwi.10.19):
${cmd}
EOF

    cat > "${artifact_dir}/repro.env" <<EOF
# Source this file, then run:
#   ./scripts/e2e_test.sh ${E2E_SUITE}
# Original artifact timestamp (for reference): ${E2E_TIMESTAMP}
AM_E2E_KEEP_TMP=1
E2E_CLOCK_MODE=${E2E_CLOCK_MODE}
E2E_SEED=${E2E_SEED}
E2E_RUN_STARTED_AT=${E2E_RUN_STARTED_AT}
E2E_RUN_START_EPOCH_S=${E2E_RUN_START_EPOCH_S}
EOF

    local seed_json="0"
    if [[ "${E2E_SEED}" =~ ^[0-9]+$ ]]; then
        seed_json="${E2E_SEED}"
    fi

    local start_epoch_json="0"
    if [[ "${E2E_RUN_START_EPOCH_S}" =~ ^[0-9]+$ ]]; then
        start_epoch_json="${E2E_RUN_START_EPOCH_S}"
    fi

    cat > "${artifact_dir}/repro.json" <<EOJSON
{
  "schema_version": 1,
  "suite": "$( _e2e_json_escape "$E2E_SUITE" )",
  "timestamp": "$( _e2e_json_escape "$E2E_TIMESTAMP" )",
  "clock_mode": "$( _e2e_json_escape "$E2E_CLOCK_MODE" )",
  "seed": ${seed_json},
  "run_started_at": "$( _e2e_json_escape "$E2E_RUN_STARTED_AT" )",
  "run_start_epoch_s": ${start_epoch_json},
  "command": "$( _e2e_json_escape "$cmd" )"
}
EOJSON
}

_e2e_now_rfc3339() {
    if [ "${E2E_CLOCK_MODE:-wall}" = "deterministic" ]; then
        local epoch="${E2E_RUN_START_EPOCH_S}"
        epoch=$(( epoch + _E2E_TRACE_SEQ ))
        (( _E2E_TRACE_SEQ++ )) || true
        date -u -d "@${epoch}" '+%Y-%m-%dT%H:%M:%SZ' 2>/dev/null || date -u '+%Y-%m-%dT%H:%M:%SZ'
        return 0
    fi

    date -u '+%Y-%m-%dT%H:%M:%SZ'
}

_e2e_trace_event() {
    local kind="$1"
    local msg="${2:-}"
    local case_name="${3:-${_E2E_CURRENT_CASE:-}}"

    if [ -z "${_E2E_TRACE_FILE:-}" ]; then
        return 0
    fi

    mkdir -p "$(dirname "$_E2E_TRACE_FILE")"

    local ts
    ts="$(_e2e_now_rfc3339)"

    local safe_suite safe_run_ts safe_ts safe_kind safe_case safe_msg
    safe_suite="$(_e2e_json_escape "$E2E_SUITE")"
    safe_run_ts="$(_e2e_json_escape "$E2E_TIMESTAMP")"
    safe_ts="$(_e2e_json_escape "$ts")"
    safe_kind="$(_e2e_json_escape "$kind")"
    safe_case="$(_e2e_json_escape "$case_name")"
    safe_msg="$(_e2e_json_escape "$msg")"

    echo "{\"schema_version\":1,\"suite\":\"${safe_suite}\",\"run_timestamp\":\"${safe_run_ts}\",\"ts\":\"${safe_ts}\",\"kind\":\"${safe_kind}\",\"case\":\"${safe_case}\",\"message\":\"${safe_msg}\",\"counters\":{\"total\":${_E2E_TOTAL},\"pass\":${_E2E_PASS},\"fail\":${_E2E_FAIL},\"skip\":${_E2E_SKIP}}}" >>"$_E2E_TRACE_FILE"
}

e2e_write_summary_json() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    cat > "${artifact_dir}/summary.json" <<EOJSON
{
  "schema_version": 1,
  "suite": "$( _e2e_json_escape "$E2E_SUITE" )",
  "timestamp": "$( _e2e_json_escape "$E2E_TIMESTAMP" )",
  "started_at": "$( _e2e_json_escape "$E2E_RUN_STARTED_AT" )",
  "ended_at": "$( _e2e_json_escape "$E2E_RUN_ENDED_AT" )",
  "total": ${_E2E_TOTAL},
  "pass": ${_E2E_PASS},
  "fail": ${_E2E_FAIL},
  "skip": ${_E2E_SKIP}
}
EOJSON
}

e2e_write_meta_json() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"

    local git_commit=""
    local git_branch=""
    local git_dirty="false"
    git_commit="$(git -C "$E2E_PROJECT_ROOT" rev-parse HEAD 2>/dev/null || echo "")"
    git_branch="$(git -C "$E2E_PROJECT_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")"
    if ! git -C "$E2E_PROJECT_ROOT" diff --quiet 2>/dev/null || ! git -C "$E2E_PROJECT_ROOT" diff --cached --quiet 2>/dev/null; then
        git_dirty="true"
    fi

    local user host os arch bash_ver py_ver
    user="$(whoami 2>/dev/null || echo "")"
    host="$(hostname 2>/dev/null || echo "")"
    os="$(uname -s 2>/dev/null || echo "")"
    arch="$(uname -m 2>/dev/null || echo "")"
    bash_ver="${BASH_VERSION:-}"
    py_ver=""
    if command -v python3 >/dev/null 2>&1; then
        py_ver="$(python3 --version 2>&1 || true)"
    elif command -v python >/dev/null 2>&1; then
        py_ver="$(python --version 2>&1 || true)"
    fi

    local seed_json="0"
    if [[ "${E2E_SEED}" =~ ^[0-9]+$ ]]; then
        seed_json="${E2E_SEED}"
    fi

    local start_epoch_json="0"
    if [[ "${E2E_RUN_START_EPOCH_S}" =~ ^[0-9]+$ ]]; then
        start_epoch_json="${E2E_RUN_START_EPOCH_S}"
    fi

    cat > "${artifact_dir}/meta.json" <<EOJSON
{
  "schema_version": 1,
  "suite": "$( _e2e_json_escape "$E2E_SUITE" )",
  "timestamp": "$( _e2e_json_escape "$E2E_TIMESTAMP" )",
  "started_at": "$( _e2e_json_escape "$E2E_RUN_STARTED_AT" )",
  "ended_at": "$( _e2e_json_escape "$E2E_RUN_ENDED_AT" )",
  "git": {
    "commit": "$( _e2e_json_escape "$git_commit" )",
    "branch": "$( _e2e_json_escape "$git_branch" )",
    "dirty": ${git_dirty}
  },
  "runner": {
    "user": "$( _e2e_json_escape "$user" )",
    "hostname": "$( _e2e_json_escape "$host" )",
    "os": "$( _e2e_json_escape "$os" )",
    "arch": "$( _e2e_json_escape "$arch" )",
    "bash": "$( _e2e_json_escape "$bash_ver" )",
    "python": "$( _e2e_json_escape "$py_ver" )"
  },
  "paths": {
    "project_root": "$( _e2e_json_escape "$E2E_PROJECT_ROOT" )",
    "artifact_dir": "$( _e2e_json_escape "$artifact_dir" )"
  },
  "determinism": {
    "clock_mode": "$( _e2e_json_escape "$E2E_CLOCK_MODE" )",
    "seed": ${seed_json},
    "run_start_epoch_s": ${start_epoch_json}
  }
}
EOJSON
}

e2e_write_metrics_json() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"

    local duration_s=0
    if [ "$E2E_RUN_END_EPOCH_S" -ge "$E2E_RUN_START_EPOCH_S" ] 2>/dev/null; then
        duration_s=$(( E2E_RUN_END_EPOCH_S - E2E_RUN_START_EPOCH_S ))
    fi

    cat > "${artifact_dir}/metrics.json" <<EOJSON
{
  "schema_version": 1,
  "suite": "$( _e2e_json_escape "$E2E_SUITE" )",
  "timestamp": "$( _e2e_json_escape "$E2E_TIMESTAMP" )",
  "timing": {
    "start_epoch_s": ${E2E_RUN_START_EPOCH_S},
    "end_epoch_s": ${E2E_RUN_END_EPOCH_S},
    "duration_s": ${duration_s}
  },
  "counts": {
    "total": ${_E2E_TOTAL},
    "pass": ${_E2E_PASS},
    "fail": ${_E2E_FAIL},
    "skip": ${_E2E_SKIP}
  }
}
EOJSON
}

e2e_write_diagnostics_files() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    local diag_dir="${artifact_dir}/diagnostics"
    mkdir -p "$diag_dir"

    local env_file="${diag_dir}/env_redacted.txt"
    {
        echo "Environment (redacted):"
        e2e_dump_env 2>/dev/null || true
    } >"$env_file"

    local tree_file="${diag_dir}/tree.txt"
    local td
    td="$(e2e_mktemp "e2e_tree")"
    e2e_tree "$artifact_dir" > "${td}/tree.txt"
    cp "${td}/tree.txt" "$tree_file"
}

e2e_write_transcript_summary() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    local out="${artifact_dir}/transcript/summary.txt"
    mkdir -p "$(dirname "$out")"

    local git_commit="" git_branch="" git_dirty="false"
    git_commit="$(git -C "$E2E_PROJECT_ROOT" rev-parse HEAD 2>/dev/null || echo "")"
    git_branch="$(git -C "$E2E_PROJECT_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")"
    if ! git -C "$E2E_PROJECT_ROOT" diff --quiet 2>/dev/null || ! git -C "$E2E_PROJECT_ROOT" diff --cached --quiet 2>/dev/null; then
        git_dirty="true"
    fi

    {
        echo "suite: ${E2E_SUITE}"
        echo "timestamp: ${E2E_TIMESTAMP}"
        echo "started_at: ${E2E_RUN_STARTED_AT}"
        echo "ended_at: ${E2E_RUN_ENDED_AT}"
        echo "clock_mode: ${E2E_CLOCK_MODE}"
        echo "seed: ${E2E_SEED}"
        echo "run_start_epoch_s: ${E2E_RUN_START_EPOCH_S}"
        echo "repro_command: $(e2e_repro_command | tr -d '\n')"
        echo "counts: total=${_E2E_TOTAL} pass=${_E2E_PASS} fail=${_E2E_FAIL} skip=${_E2E_SKIP}"
        echo "git: commit=${git_commit} branch=${git_branch} dirty=${git_dirty}"
        echo "artifacts_dir: ${artifact_dir}"
        echo "files:"
        echo "  bundle: bundle.json"
        echo "  summary: summary.json"
        echo "  meta: meta.json"
        echo "  metrics: metrics.json"
        echo "  trace: trace/events.jsonl"
        echo "  repro: repro.txt"
        echo "  repro_json: repro.json"
        echo "  env: diagnostics/env_redacted.txt"
        echo "  tree: diagnostics/tree.txt"
    } >"$out"
}

e2e_write_bundle_manifest() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    if [ ! -d "$artifact_dir" ]; then
        return 0
    fi

    local manifest="${artifact_dir}/bundle.json"
    local generated_at
    generated_at="$(_e2e_now_rfc3339)"

    local git_commit=""
    local git_branch=""
    local git_dirty="false"
    git_commit="$(git -C "$E2E_PROJECT_ROOT" rev-parse HEAD 2>/dev/null || echo "")"
    git_branch="$(git -C "$E2E_PROJECT_ROOT" rev-parse --abbrev-ref HEAD 2>/dev/null || echo "")"
    if ! git -C "$E2E_PROJECT_ROOT" diff --quiet 2>/dev/null || ! git -C "$E2E_PROJECT_ROOT" diff --cached --quiet 2>/dev/null; then
        git_dirty="true"
    fi

    {
        echo "{"
        echo "  \"schema\": {\"name\": \"mcp-agent-mail-artifacts\", \"major\": 1, \"minor\": 0},"
        echo "  \"suite\": \"$( _e2e_json_escape "$E2E_SUITE" )\","
        echo "  \"timestamp\": \"$( _e2e_json_escape "$E2E_TIMESTAMP" )\","
        echo "  \"generated_at\": \"$( _e2e_json_escape "$generated_at" )\","
        echo "  \"started_at\": \"$( _e2e_json_escape "$E2E_RUN_STARTED_AT" )\","
        echo "  \"ended_at\": \"$( _e2e_json_escape "$E2E_RUN_ENDED_AT" )\","
        echo "  \"counts\": {\"total\": ${_E2E_TOTAL}, \"pass\": ${_E2E_PASS}, \"fail\": ${_E2E_FAIL}, \"skip\": ${_E2E_SKIP}},"
        echo "  \"git\": {\"commit\": \"$( _e2e_json_escape "$git_commit" )\", \"branch\": \"$( _e2e_json_escape "$git_branch" )\", \"dirty\": ${git_dirty}},"
        echo "  \"artifacts\": {"
        echo "    \"metadata\": {\"path\": \"meta.json\", \"schema\": \"meta.v1\"},"
        echo "    \"metrics\": {\"path\": \"metrics.json\", \"schema\": \"metrics.v1\"},"
        echo "    \"summary\": {\"path\": \"summary.json\", \"schema\": \"summary.v1\"},"
        echo "    \"diagnostics\": {"
        echo "      \"env_redacted\": {\"path\": \"diagnostics/env_redacted.txt\"},"
        echo "      \"tree\": {\"path\": \"diagnostics/tree.txt\"}"
        echo "    },"
        echo "    \"trace\": {\"events\": {\"path\": \"trace/events.jsonl\", \"schema\": \"trace-events.v1\"}},"
        echo "    \"transcript\": {\"summary\": {\"path\": \"transcript/summary.txt\"}}"
        echo "  },"
        echo "  \"files\": ["

        local first=1
        while IFS= read -r f; do
            local rel="${f#"$artifact_dir"/}"
            local sha
            sha="$(e2e_sha256 "$f")"
            local bytes
            bytes="$(_e2e_stat_bytes "$f")"

            local kind="opaque"
            local schema_json="null"
            case "$rel" in
                summary.json)
                    kind="metrics"
                    schema_json="\"summary.v1\""
                    ;;
                meta.json)
                    kind="metadata"
                    schema_json="\"meta.v1\""
                    ;;
                metrics.json)
                    kind="metrics"
                    schema_json="\"metrics.v1\""
                    ;;
                trace/events.jsonl)
                    kind="trace"
                    schema_json="\"trace-events.v1\""
                    ;;
                diagnostics/*)
                    kind="diagnostics"
                    ;;
                transcript/*)
                    kind="transcript"
                    ;;
                steps/step_*.json)
                    kind="trace"
                    schema_json="\"step.v1\""
                    ;;
                failures/fail_*.json)
                    kind="diagnostics"
                    schema_json="\"failure.v1\""
                    ;;
            esac

            if [ "$first" -eq 1 ]; then
                first=0
            else
                echo "    ,"
            fi
            echo "    {\"path\": \"$( _e2e_json_escape "$rel" )\", \"sha256\": \"$( _e2e_json_escape "$sha" )\", \"bytes\": ${bytes}, \"kind\": \"$( _e2e_json_escape "$kind" )\", \"schema\": ${schema_json}}"
        done < <(find "$artifact_dir" -type f ! -name "bundle.json" | sort)

        echo "  ]"
        echo "}"
    } >"$manifest"
}

e2e_validate_bundle_manifest() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    local manifest="${artifact_dir}/bundle.json"
    if [ ! -f "$manifest" ]; then
        e2e_log "bundle.json missing at ${manifest}"
        return 1
    fi

    # Prefer python3 for strict structural validation; fall back to python.
    local py="python3"
    if ! command -v "$py" >/dev/null 2>&1; then
        py="python"
    fi
    if command -v "$py" >/dev/null 2>&1; then
        "$py" - "$artifact_dir" <<'PY'
import json
import os
import re
import sys

artifact_dir = sys.argv[1]
manifest_path = os.path.join(artifact_dir, "bundle.json")

with open(manifest_path, "r", encoding="utf-8") as f:
    bundle = json.load(f)

def fail(msg: str) -> None:
    raise SystemExit(msg)

def require(obj, key, kind=None):
    if not isinstance(obj, dict):
        fail("expected object")
    if key not in obj:
        fail(f"missing key: {key}")
    val = obj[key]
    if kind is not None and not isinstance(val, kind):
        fail(f"bad type for {key}: expected {kind.__name__}")
    return val

def require_bool(obj, key):
    val = require(obj, key)
    if type(val) is not bool:
        fail(f"bad type for {key}: expected bool")
    return val

schema = require(bundle, "schema", dict)
name = require(schema, "name", str)
major = require(schema, "major", int)
minor = require(schema, "minor", int)
if name != "mcp-agent-mail-artifacts":
    fail(f"unsupported schema.name={name}")
if major != 1:
    fail(f"unsupported schema.major={major}")
if minor < 0:
    fail("schema.minor must be >= 0")

suite = require(bundle, "suite", str)
timestamp = require(bundle, "timestamp", str)
require(bundle, "generated_at", str)
require(bundle, "started_at", str)
require(bundle, "ended_at", str)

counts = require(bundle, "counts", dict)
for k in ("total", "pass", "fail", "skip"):
    require(counts, k, int)

git = require(bundle, "git", dict)
require(git, "commit", str)
require(git, "branch", str)
require_bool(git, "dirty")

artifacts = require(bundle, "artifacts", dict)
required_artifact_paths = []

def req_path(obj, key, require_schema=False):
    ent = require(obj, key, dict)
    path = require(ent, "path", str)
    if require_schema:
        require(ent, "schema", str)
    required_artifact_paths.append(path)
    return ent

req_path(artifacts, "metadata", True)
req_path(artifacts, "metrics", True)
req_path(artifacts, "summary", True)

diag = require(artifacts, "diagnostics", dict)
req_path(diag, "env_redacted")
req_path(diag, "tree")

trace = require(artifacts, "trace", dict)
events = require(trace, "events", dict)
required_artifact_paths.append(require(events, "path", str))
require(events, "schema", str)

transcript = require(artifacts, "transcript", dict)
req_path(transcript, "summary")

files = require(bundle, "files", list)
file_map = {}
allowed_kinds = {"metadata", "metrics", "diagnostics", "trace", "transcript", "opaque"}
sha_re = re.compile(r"^[0-9a-f]{64}$")

for i, ent in enumerate(files):
    if not isinstance(ent, dict):
        fail(f"files[{i}] must be object")
    path = require(ent, "path", str)
    if path.startswith("/") or path.startswith("\\"):
        fail(f"files[{i}].path must be relative")
    if ".." in path.split("/"):
        fail(f"files[{i}].path must not contain ..")
    if path in file_map:
        fail(f"duplicate path in files: {path}")
    sha = require(ent, "sha256", str)
    if not sha_re.match(sha):
        fail(f"files[{i}].sha256 must be 64 lowercase hex chars")
    b = require(ent, "bytes", int)
    if b < 0:
        fail(f"files[{i}].bytes must be >= 0")
    kind = require(ent, "kind", str)
    if kind not in allowed_kinds:
        fail(f"files[{i}].kind invalid: {kind}")
    schema_val = ent.get("schema", None)
    if schema_val is not None and not isinstance(schema_val, str):
        fail(f"files[{i}].schema must be string or null")

    file_map[path] = ent

for p in required_artifact_paths:
    if p not in file_map:
        fail(f"required file missing from bundle.files: {p}")

# Verify referenced files exist and bytes match.
for path, ent in file_map.items():
    abs_path = os.path.join(artifact_dir, path)
    if not os.path.isfile(abs_path):
        fail(f"missing file on disk: {path}")
    actual_bytes = os.path.getsize(abs_path)
    if actual_bytes != ent["bytes"]:
        fail(f"bytes mismatch for {path}: manifest={ent['bytes']} actual={actual_bytes}")

def load_json(rel_path: str):
    with open(os.path.join(artifact_dir, rel_path), "r", encoding="utf-8") as f:
        return json.load(f)

# Required JSON artifacts (schema checks)
summary = load_json("summary.json")
require(summary, "schema_version", int)
if require(summary, "suite", str) != suite:
    fail("summary.json suite mismatch")
if require(summary, "timestamp", str) != timestamp:
    fail("summary.json timestamp mismatch")
require(summary, "started_at", str)
require(summary, "ended_at", str)
for k in ("total", "pass", "fail", "skip"):
    require(summary, k, int)

meta = load_json("meta.json")
require(meta, "schema_version", int)
if require(meta, "suite", str) != suite:
    fail("meta.json suite mismatch")
if require(meta, "timestamp", str) != timestamp:
    fail("meta.json timestamp mismatch")
require(meta, "started_at", str)
require(meta, "ended_at", str)
require(require(meta, "git", dict), "commit", str)
require(require(meta, "git", dict), "branch", str)
require_bool(require(meta, "git", dict), "dirty")

metrics = load_json("metrics.json")
require(metrics, "schema_version", int)
if require(metrics, "suite", str) != suite:
    fail("metrics.json suite mismatch")
if require(metrics, "timestamp", str) != timestamp:
    fail("metrics.json timestamp mismatch")
timing = require(metrics, "timing", dict)
require(timing, "start_epoch_s", int)
require(timing, "end_epoch_s", int)
require(timing, "duration_s", int)
mc = require(metrics, "counts", dict)
for k in ("total", "pass", "fail", "skip"):
    require(mc, k, int)
    if mc[k] != counts[k]:
        fail(f"metrics.json counts.{k} mismatch")

# Parse and validate trace events JSONL
events_path = os.path.join(artifact_dir, "trace", "events.jsonl")
seen_start = False
seen_end = False
with open(events_path, "r", encoding="utf-8") as f:
    for ln, line in enumerate(f, 1):
        line = line.strip()
        if not line:
            continue
        try:
            ev = json.loads(line)
        except Exception as e:
            fail(f"trace/events.jsonl invalid JSON at line {ln}: {e}")
        if not isinstance(ev, dict):
            fail(f"trace/events.jsonl line {ln}: expected object")
        if ev.get("schema_version") != 1:
            fail(f"trace/events.jsonl line {ln}: schema_version must be 1")
        if ev.get("suite") != suite:
            fail(f"trace/events.jsonl line {ln}: suite mismatch")
        if ev.get("run_timestamp") != timestamp:
            fail(f"trace/events.jsonl line {ln}: run_timestamp mismatch")
        if not isinstance(ev.get("ts"), str):
            fail(f"trace/events.jsonl line {ln}: ts must be string")
        kind = ev.get("kind")
        if not isinstance(kind, str):
            fail(f"trace/events.jsonl line {ln}: kind must be string")
        if kind == "suite_start":
            seen_start = True
        if kind == "suite_end":
            seen_end = True
        if not isinstance(ev.get("case"), str):
            fail(f"trace/events.jsonl line {ln}: case must be string")
        if not isinstance(ev.get("message"), str):
            fail(f"trace/events.jsonl line {ln}: message must be string")
        ctr = ev.get("counters")
        if not isinstance(ctr, dict):
            fail(f"trace/events.jsonl line {ln}: counters must be object")
        for k in ("total", "pass", "fail", "skip"):
            if not isinstance(ctr.get(k), int):
                fail(f"trace/events.jsonl line {ln}: counters.{k} must be int")

if not seen_start:
    fail("trace/events.jsonl missing suite_start")
if not seen_end:
    fail("trace/events.jsonl missing suite_end")

# Generic parseability checks for JSON/JSONL artifacts.
for path in file_map.keys():
    abs_path = os.path.join(artifact_dir, path)
    if path.endswith(".json"):
        try:
            with open(abs_path, "r", encoding="utf-8") as f:
                txt = f.read()
            # Some suites intentionally capture empty bodies into *.json artifacts.
            # Treat empty/whitespace-only files as valid "no payload" transcripts.
            if not txt.strip():
                continue
            json.loads(txt)
        except Exception as e:
            fail(f"{path} invalid JSON: {e}")
    if path.endswith(".jsonl") or path.endswith(".ndjson"):
        with open(abs_path, "r", encoding="utf-8") as f:
            for ln, line in enumerate(f, 1):
                line = line.strip()
                if not line:
                    continue
                try:
                    json.loads(line)
                except Exception as e:
                    fail(f"{path} invalid JSONL at line {ln}: {e}")
PY
        return $?
    fi

    # Fallback: shallow sanity check (no JSON parser available).
    grep -q '"schema"' "$manifest" && grep -q '"files"' "$manifest" && grep -q '"artifacts"' "$manifest"
}

# ---------------------------------------------------------------------------
# File tree and hashing helpers
# ---------------------------------------------------------------------------

# Dump a directory tree (sorted, deterministic)
e2e_tree() {
    local dir="$1"
    find "$dir" -type f | sort | while read -r f; do
        local rel="${f#"$dir"/}"
        local sz
        sz=$(stat --format='%s' "$f" 2>/dev/null || stat -f '%z' "$f" 2>/dev/null || echo "?")
        echo "${rel} (${sz}b)"
    done
}

# Stable SHA256 of a file
e2e_sha256() {
    local file="$1"
    sha256sum "$file" 2>/dev/null | awk '{print $1}' || shasum -a 256 "$file" | awk '{print $1}'
}

# Stable SHA256 of a string
e2e_sha256_str() {
    local str="$1"
    echo -n "$str" | sha256sum 2>/dev/null | awk '{print $1}' || echo -n "$str" | shasum -a 256 | awk '{print $1}'
}

# ---------------------------------------------------------------------------
# Retry helper
# ---------------------------------------------------------------------------

# Retry a command with exponential backoff
# Usage: e2e_retry <max_attempts> <initial_delay_ms> <command...>
e2e_retry() {
    local max_attempts="$1"
    local delay_ms="$2"
    shift 2
    local attempt=1
    while [ $attempt -le "$max_attempts" ]; do
        if "$@"; then
            return 0
        fi
        if [ $attempt -eq "$max_attempts" ]; then
            return 1
        fi
        local delay_s
        delay_s=$(echo "scale=3; $delay_ms / 1000" | bc 2>/dev/null || echo "0.5")
        sleep "$delay_s"
        delay_ms=$(( delay_ms * 2 ))
        (( attempt++ )) || true
    done
    return 1
}

# Wait for a TCP port to become available
e2e_wait_port() {
    local host="${1:-127.0.0.1}"
    local port="$2"
    local timeout_s="${3:-10}"
    local deadline
    deadline=$(( $(date +%s) + timeout_s ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if bash -c "echo > /dev/tcp/${host}/${port}" 2>/dev/null; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# ---------------------------------------------------------------------------
# Environment dump (redact secrets)
# ---------------------------------------------------------------------------

e2e_dump_env() {
    e2e_log "Environment:"
    env | sort | while read -r line; do
        local key="${line%%=*}"
        local val="${line#*=}"
        # Redact anything that looks like a secret
        case "$key" in
            *SECRET*|*TOKEN*|*PASSWORD*|*KEY*|*CREDENTIAL*|*AUTH*)
                echo "  ${key}=<redacted>"
                ;;
            *)
                echo "  ${key}=${val}"
                ;;
        esac
    done
}

# ---------------------------------------------------------------------------
# Git helpers (safe, temp-dir only)
# ---------------------------------------------------------------------------

# Initialize a fresh git repo in a temp dir
e2e_init_git_repo() {
    local dir="$1"
    git -C "$dir" init -q
    git -C "$dir" config user.email "e2e@test.local"
    git -C "$dir" config user.name "E2E Test"
}

# Create a commit in a test repo
e2e_git_commit() {
    local dir="$1"
    local msg="${2:-test commit}"
    git -C "$dir" add -A
    git -C "$dir" commit -qm "$msg" --allow-empty
}

# ---------------------------------------------------------------------------
# Binary helpers
# ---------------------------------------------------------------------------

# Build the workspace binary (if needed)
e2e_ensure_binary() {
    local bin_name="${1:-mcp-agent-mail}"
    local bin_path="${CARGO_TARGET_DIR}/debug/${bin_name}"
    if [ ! -f "$bin_path" ] || [ "${E2E_FORCE_BUILD:-0}" = "1" ]; then
        e2e_log "Building ${bin_name}..."
        case "$bin_name" in
            am)
                cargo build -p "mcp-agent-mail-cli" --bin "am" 2>&1 | tail -5
                ;;
            mcp-agent-mail)
                cargo build -p "mcp-agent-mail" --bin "mcp-agent-mail" 2>&1 | tail -5
                ;;
            *)
                # Default: assume package/bin share the same name.
                cargo build -p "$bin_name" --bin "$bin_name" 2>&1 | tail -5
                ;;
        esac
    fi

    # Ensure built binaries are callable by name in E2E scripts.
    export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
    echo "$bin_path"
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_summary() {
    echo ""
    echo -e "${_e2e_color_blue}════════════════════════════════════════════════════════════${_e2e_color_reset}"
    echo -e "  Suite: ${E2E_SUITE}"
    echo -e "  Total: ${_E2E_TOTAL}  ${_e2e_color_green}Pass: ${_E2E_PASS}${_e2e_color_reset}  ${_e2e_color_red}Fail: ${_E2E_FAIL}${_e2e_color_reset}  ${_e2e_color_yellow}Skip: ${_E2E_SKIP}${_e2e_color_reset}"
    echo -e "  Artifacts: ${E2E_ARTIFACT_DIR}"
    echo -e "${_e2e_color_blue}════════════════════════════════════════════════════════════${_e2e_color_reset}"

    E2E_RUN_ENDED_AT="$(_e2e_now_rfc3339)"
    if [ "${E2E_CLOCK_MODE:-wall}" = "deterministic" ]; then
        # _e2e_now_rfc3339 advances _E2E_TRACE_SEQ by 1.
        E2E_RUN_END_EPOCH_S=$(( E2E_RUN_START_EPOCH_S + _E2E_TRACE_SEQ - 1 ))
    else
        E2E_RUN_END_EPOCH_S="$(date +%s)"
    fi
    _e2e_trace_event "suite_end" ""

    # Save summary to artifacts
    if [ -d "$E2E_ARTIFACT_DIR" ]; then
        e2e_write_summary_json
        e2e_write_meta_json
        e2e_write_metrics_json
        e2e_write_diagnostics_files
        e2e_write_transcript_summary
        e2e_write_repro_files

        # Emit a versioned bundle manifest and validate it. This provides
        # artifact-contract enforcement for CI regression triage (br-3vwi.10.18).
        e2e_write_bundle_manifest
        if ! e2e_validate_bundle_manifest; then
            e2e_log "Artifact bundle manifest validation failed"
            return 1
        fi
    fi

    if [ "$_E2E_FAIL" -gt 0 ]; then
        echo "" >&2
        echo "[e2e] Repro:" >&2
        e2e_repro_command >&2
        echo "" >&2
        return 1
    fi
    return 0
}
