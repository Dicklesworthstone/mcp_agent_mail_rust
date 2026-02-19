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
# Active case marker context for e2e_mark_case_start/e2e_mark_case_end dedupe.
_E2E_MARKER_ACTIVE_CASE=""

# Per-assertion ID tracking (br-1xt0m.1.13.13)
# Auto-incremented within each case; reset by e2e_case_banner.
_E2E_ASSERT_SEQ=0

# Step tracking (br-1xt0m.1.13.13)
# Optional structured step within a case. Set via e2e_step_start/e2e_step_end.
_E2E_CURRENT_STEP=""
_E2E_STEP_START_MS=0

# Case timing (br-1xt0m.1.13.13)
_E2E_CASE_START_MS=0

# Optional fixture identifiers gathered by suites/harness.
# Suites can append with e2e_add_fixture_id; environment injection is also
# supported via E2E_FIXTURE_IDS (comma/space-separated).
_E2E_FIXTURE_IDS=()

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
    if [ -n "${_E2E_MARKER_ACTIVE_CASE:-}" ] && [ "${_E2E_MARKER_ACTIVE_CASE}" != "$case_name" ]; then
        e2e_mark_case_end "${_E2E_MARKER_ACTIVE_CASE}"
    fi
    e2e_mark_case_start "$case_name"
    (( _E2E_TOTAL++ )) || true
    _E2E_CURRENT_CASE="$case_name"
    _E2E_ASSERT_SEQ=0
    _E2E_CURRENT_STEP=""
    _E2E_CASE_START_MS="$(_e2e_now_ms)"
    _e2e_trace_event "case_start" "" "$case_name"
    echo ""
    echo -e "${_e2e_color_blue}── Case: ${case_name} ──${_e2e_color_reset}"
}

e2e_pass() {
    local msg="${1:-}"
    (( _E2E_PASS++ )) || true
    (( _E2E_ASSERT_SEQ++ )) || true
    local aid="${_E2E_CURRENT_CASE:+${_E2E_CURRENT_CASE}.}a${_E2E_ASSERT_SEQ}"
    local now_ms elapsed=""
    now_ms="$(_e2e_now_ms)"
    if [ "${_E2E_CASE_START_MS:-0}" -gt 0 ]; then
        elapsed=$(( now_ms - _E2E_CASE_START_MS ))
    fi
    _e2e_trace_event "assert_pass" "$msg" "" "$aid" "" "$elapsed"
    echo -e "  ${_e2e_color_green}PASS${_e2e_color_reset} ${msg}"
}

e2e_fail() {
    local msg="${1:-}"
    (( _E2E_FAIL++ )) || true
    (( _E2E_ASSERT_SEQ++ )) || true
    local aid="${_E2E_CURRENT_CASE:+${_E2E_CURRENT_CASE}.}a${_E2E_ASSERT_SEQ}"
    local now_ms elapsed=""
    now_ms="$(_e2e_now_ms)"
    if [ "${_E2E_CASE_START_MS:-0}" -gt 0 ]; then
        elapsed=$(( now_ms - _E2E_CASE_START_MS ))
    fi
    _e2e_trace_event "assert_fail" "$msg" "" "$aid" "" "$elapsed"
    echo -e "  ${_e2e_color_red}FAIL${_e2e_color_reset} ${msg}"
}

e2e_skip() {
    local msg="${1:-}"
    (( _E2E_SKIP++ )) || true
    (( _E2E_ASSERT_SEQ++ )) || true
    local aid="${_E2E_CURRENT_CASE:+${_E2E_CURRENT_CASE}.}a${_E2E_ASSERT_SEQ}"
    _e2e_trace_event "assert_skip" "$msg" "" "$aid"
    echo -e "  ${_e2e_color_yellow}SKIP${_e2e_color_reset} ${msg}"
}

# Step tracking (br-1xt0m.1.13.13)
# Wraps a logical step within a test case. Steps appear as "step" fields in trace
# events. Call e2e_step_end to emit a step_end event with elapsed_ms.
e2e_step_start() {
    local step_name="$1"
    _E2E_CURRENT_STEP="$step_name"
    _E2E_STEP_START_MS="$(_e2e_now_ms)"
    _e2e_trace_event "step_start" "" "" "" "$step_name"
}

e2e_step_end() {
    local step_name="${1:-${_E2E_CURRENT_STEP:-}}"
    local now_ms elapsed=""
    now_ms="$(_e2e_now_ms)"
    if [ "${_E2E_STEP_START_MS:-0}" -gt 0 ]; then
        elapsed=$(( now_ms - _E2E_STEP_START_MS ))
    fi
    _e2e_trace_event "step_end" "" "" "" "$step_name" "$elapsed"
    _E2E_CURRENT_STEP=""
    _E2E_STEP_START_MS=0
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
    mkdir -p "$E2E_ARTIFACT_DIR"/{diagnostics,trace,transcript,logs,screenshots}
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

e2e_add_fixture_id() {
    local fixture_id="${1:-}"
    if [ -z "$fixture_id" ]; then
        return 0
    fi
    _E2E_FIXTURE_IDS+=("$fixture_id")
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

e2e_write_fixture_ids_json() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    local out="${artifact_dir}/fixtures.json"
    local td
    td="$(e2e_mktemp "e2e_fixtures")"
    local list_file="${td}/fixture_ids.txt"
    : >"$list_file"

    local id
    for id in "${_E2E_FIXTURE_IDS[@]}"; do
        if [ -n "$id" ]; then
            printf '%s\n' "$id" >>"$list_file"
        fi
    done

    local env_ids="${E2E_FIXTURE_IDS:-}"
    if [ -n "$env_ids" ]; then
        local normalized
        normalized="$(printf '%s' "$env_ids" | tr ',;' '  ')"
        local token
        for token in $normalized; do
            if [ -n "$token" ]; then
                printf '%s\n' "$token" >>"$list_file"
            fi
        done
    fi

    if [ -d "${artifact_dir}/fixtures" ]; then
        while IFS= read -r f; do
            local rel="${f#"$artifact_dir"/}"
            printf '%s\n' "$rel" >>"$list_file"
        done < <(find "${artifact_dir}/fixtures" -type f | sort)
    fi

    sort -u "$list_file" -o "$list_file"

    {
        echo "{"
        echo "  \"schema_version\": 1,"
        echo "  \"suite\": \"$( _e2e_json_escape "$E2E_SUITE" )\","
        echo "  \"timestamp\": \"$( _e2e_json_escape "$E2E_TIMESTAMP" )\","
        echo "  \"fixture_ids\": ["
        local first=1
        while IFS= read -r line; do
            [ -z "$line" ] && continue
            if [ "$first" -eq 1 ]; then
                first=0
            else
                echo "    ,"
            fi
            echo "    \"$( _e2e_json_escape "$line" )\""
        done <"$list_file"
        echo "  ]"
        echo "}"
    } >"$out"
}

e2e_write_logs_index_json() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    local out="${artifact_dir}/logs/index.json"
    mkdir -p "$(dirname "$out")"

    local td
    td="$(e2e_mktemp "e2e_logs_index")"
    local list_file="${td}/logs.txt"
    : >"$list_file"

    while IFS= read -r f; do
        local rel="${f#"$artifact_dir"/}"
        if [ "$rel" = "logs/index.json" ]; then
            continue
        fi
        printf '%s\n' "$rel" >>"$list_file"
    done < <(find "$artifact_dir" -type f \( -name "*.log" -o -name "*.log.*" \) | sort)

    {
        echo "{"
        echo "  \"schema_version\": 1,"
        echo "  \"suite\": \"$( _e2e_json_escape "$E2E_SUITE" )\","
        echo "  \"timestamp\": \"$( _e2e_json_escape "$E2E_TIMESTAMP" )\","
        echo "  \"files\": ["
        local first=1
        while IFS= read -r rel; do
            [ -z "$rel" ] && continue
            local abs="${artifact_dir}/${rel}"
            local bytes sha
            bytes="$(_e2e_stat_bytes "$abs")"
            sha="$(e2e_sha256 "$abs")"
            if [ "$first" -eq 1 ]; then
                first=0
            else
                echo "    ,"
            fi
            echo "    {\"path\": \"$( _e2e_json_escape "$rel" )\", \"bytes\": ${bytes}, \"sha256\": \"$( _e2e_json_escape "$sha" )\"}"
        done <"$list_file"
        echo "  ]"
        echo "}"
    } >"$out"
}

e2e_write_screenshots_index_json() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    local out="${artifact_dir}/screenshots/index.json"
    mkdir -p "$(dirname "$out")"

    local td
    td="$(e2e_mktemp "e2e_screenshots_index")"
    local list_file="${td}/screenshots.txt"
    : >"$list_file"

    while IFS= read -r f; do
        local rel="${f#"$artifact_dir"/}"
        printf '%s\n' "$rel" >>"$list_file"
    done < <(find "$artifact_dir" -type f \( -iname "*.png" -o -iname "*.jpg" -o -iname "*.jpeg" -o -iname "*.webp" -o -iname "*.gif" -o -iname "*.bmp" \) | sort)

    {
        echo "{"
        echo "  \"schema_version\": 1,"
        echo "  \"suite\": \"$( _e2e_json_escape "$E2E_SUITE" )\","
        echo "  \"timestamp\": \"$( _e2e_json_escape "$E2E_TIMESTAMP" )\","
        echo "  \"files\": ["
        local first=1
        while IFS= read -r rel; do
            [ -z "$rel" ] && continue
            local abs="${artifact_dir}/${rel}"
            local bytes sha
            bytes="$(_e2e_stat_bytes "$abs")"
            sha="$(e2e_sha256 "$abs")"
            if [ "$first" -eq 1 ]; then
                first=0
            else
                echo "    ,"
            fi
            echo "    {\"path\": \"$( _e2e_json_escape "$rel" )\", \"bytes\": ${bytes}, \"sha256\": \"$( _e2e_json_escape "$sha" )\"}"
        done <"$list_file"
        echo "  ]"
        echo "}"
    } >"$out"
}

e2e_write_forensic_indexes() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    e2e_write_fixture_ids_json "$artifact_dir"
    e2e_write_logs_index_json "$artifact_dir"
    e2e_write_screenshots_index_json "$artifact_dir"
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

# Monotonic millisecond clock for elapsed_ms tracking (br-1xt0m.1.13.13).
# Returns milliseconds since epoch. In deterministic mode returns synthetic
# values derived from _E2E_TRACE_SEQ (1000ms per tick).
_e2e_now_ms() {
    if [ "${E2E_CLOCK_MODE:-wall}" = "deterministic" ]; then
        echo $(( (E2E_RUN_START_EPOCH_S + _E2E_TRACE_SEQ) * 1000 ))
        return 0
    fi
    # Try nanosecond-resolution date first (GNU coreutils), fall back to second.
    local ns
    ns="$(date +%s%N 2>/dev/null)" || true
    if [ -n "$ns" ] && [ ${#ns} -gt 10 ]; then
        echo $(( ns / 1000000 ))
    else
        echo $(( $(date +%s) * 1000 ))
    fi
}

_e2e_trace_event() {
    local kind="$1"
    local msg="${2:-}"
    local case_name="${3:-${_E2E_CURRENT_CASE:-}}"
    local assertion_id="${4:-}"
    local step_name="${5:-${_E2E_CURRENT_STEP:-}}"
    local elapsed_ms="${6:-}"

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

    # Build optional v2 fields (br-1xt0m.1.13.13)
    local v2_fields=""
    if [ -n "$assertion_id" ]; then
        v2_fields="${v2_fields},\"assertion_id\":\"$(_e2e_json_escape "$assertion_id")\""
    fi
    if [ -n "$step_name" ]; then
        v2_fields="${v2_fields},\"step\":\"$(_e2e_json_escape "$step_name")\""
    fi
    if [ -n "$elapsed_ms" ]; then
        v2_fields="${v2_fields},\"elapsed_ms\":${elapsed_ms}"
    fi

    echo "{\"schema_version\":2,\"suite\":\"${safe_suite}\",\"run_timestamp\":\"${safe_run_ts}\",\"ts\":\"${safe_ts}\",\"kind\":\"${safe_kind}\",\"case\":\"${safe_case}\",\"message\":\"${safe_msg}\",\"counters\":{\"total\":${_E2E_TOTAL},\"pass\":${_E2E_PASS},\"fail\":${_E2E_FAIL},\"skip\":${_E2E_SKIP}}${v2_fields}}" >>"$_E2E_TRACE_FILE"
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
        echo "  logs_index: logs/index.json"
        echo "  screenshots_index: screenshots/index.json"
        echo "  fixtures: fixtures.json"
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
        echo "    \"trace\": {\"events\": {\"path\": \"trace/events.jsonl\", \"schema\": \"trace-events.v2\"}},"
        echo "    \"transcript\": {\"summary\": {\"path\": \"transcript/summary.txt\"}},"
        echo "    \"logs\": {\"index\": {\"path\": \"logs/index.json\", \"schema\": \"logs-index.v1\"}},"
        echo "    \"screenshots\": {\"index\": {\"path\": \"screenshots/index.json\", \"schema\": \"screenshots-index.v1\"}},"
        echo "    \"fixtures\": {\"path\": \"fixtures.json\", \"schema\": \"fixtures.v1\"},"
        echo "    \"replay\": {"
        echo "      \"command\": {\"path\": \"repro.txt\"},"
        echo "      \"environment\": {\"path\": \"repro.env\"},"
        echo "      \"metadata\": {\"path\": \"repro.json\", \"schema\": \"repro.v1\"}"
        echo "    }"
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
                    schema_json="\"trace-events.v2\""
                    ;;
                logs/index.json)
                    kind="log"
                    schema_json="\"logs-index.v1\""
                    ;;
                screenshots/index.json)
                    kind="screenshot"
                    schema_json="\"screenshots-index.v1\""
                    ;;
                fixtures.json)
                    kind="fixture"
                    schema_json="\"fixtures.v1\""
                    ;;
                repro.json)
                    kind="replay"
                    schema_json="\"repro.v1\""
                    ;;
                repro.txt|repro.env)
                    kind="replay"
                    ;;
                diagnostics/*)
                    kind="diagnostics"
                    ;;
                transcript/*)
                    kind="transcript"
                    ;;
                fixtures/*)
                    kind="fixture"
                    ;;
                *.log|*.log.*)
                    kind="log"
                    ;;
                *.png|*.jpg|*.jpeg|*.webp|*.gif|*.bmp)
                    kind="screenshot"
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

logs = require(artifacts, "logs", dict)
logs_index = require(logs, "index", dict)
required_artifact_paths.append(require(logs_index, "path", str))
require(logs_index, "schema", str)

screenshots = require(artifacts, "screenshots", dict)
screenshots_index = require(screenshots, "index", dict)
required_artifact_paths.append(require(screenshots_index, "path", str))
require(screenshots_index, "schema", str)

req_path(artifacts, "fixtures", True)

replay = require(artifacts, "replay", dict)
req_path(replay, "command")
req_path(replay, "environment")
req_path(replay, "metadata", True)

files = require(bundle, "files", list)
file_map = {}
allowed_kinds = {
    "metadata",
    "metrics",
    "diagnostics",
    "trace",
    "transcript",
    "log",
    "screenshot",
    "fixture",
    "replay",
    "opaque",
}
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

fixtures = load_json("fixtures.json")
require(fixtures, "schema_version", int)
if require(fixtures, "suite", str) != suite:
    fail("fixtures.json suite mismatch")
if require(fixtures, "timestamp", str) != timestamp:
    fail("fixtures.json timestamp mismatch")
fixture_ids = require(fixtures, "fixture_ids", list)
seen_fixture_ids = set()
for i, fid in enumerate(fixture_ids):
    if not isinstance(fid, str):
        fail(f"fixtures.json fixture_ids[{i}] must be string")
    if fid in seen_fixture_ids:
        fail(f"fixtures.json fixture_ids must be unique: {fid}")
    seen_fixture_ids.add(fid)

repro = load_json("repro.json")
require(repro, "schema_version", int)
if require(repro, "suite", str) != suite:
    fail("repro.json suite mismatch")
if require(repro, "timestamp", str) != timestamp:
    fail("repro.json timestamp mismatch")
require(repro, "clock_mode", str)
require(repro, "seed", int)
require(repro, "run_started_at", str)
require(repro, "run_start_epoch_s", int)
require(repro, "command", str)

def validate_index(rel_path: str, index_name: str):
    idx = load_json(rel_path)
    require(idx, "schema_version", int)
    if require(idx, "suite", str) != suite:
        fail(f"{index_name} suite mismatch")
    if require(idx, "timestamp", str) != timestamp:
        fail(f"{index_name} timestamp mismatch")
    entries = require(idx, "files", list)
    for i, ent in enumerate(entries):
        if not isinstance(ent, dict):
            fail(f"{index_name} files[{i}] must be object")
        p = require(ent, "path", str)
        if p not in file_map:
            fail(f"{index_name} references missing file: {p}")
        b = require(ent, "bytes", int)
        if b < 0:
            fail(f"{index_name} files[{i}].bytes must be >= 0")
        sha = require(ent, "sha256", str)
        if not sha_re.match(sha):
            fail(f"{index_name} files[{i}].sha256 must be 64 lowercase hex chars")
        if file_map[p]["bytes"] != b:
            fail(f"{index_name} bytes mismatch for {p}")
        if file_map[p]["sha256"] != sha:
            fail(f"{index_name} sha256 mismatch for {p}")

validate_index("logs/index.json", "logs/index.json")
validate_index("screenshots/index.json", "screenshots/index.json")

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
        sv = ev.get("schema_version")
        if sv not in (1, 2):
            fail(f"trace/events.jsonl line {ln}: schema_version must be 1 or 2, got {sv}")
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
        # v2 optional fields (br-1xt0m.1.13.13): assertion_id, step, elapsed_ms
        if "assertion_id" in ev and not isinstance(ev["assertion_id"], str):
            fail(f"trace/events.jsonl line {ln}: assertion_id must be string")
        if "step" in ev and not isinstance(ev["step"], str):
            fail(f"trace/events.jsonl line {ln}: step must be string")
        if "elapsed_ms" in ev and not isinstance(ev["elapsed_ms"], (int, float)):
            fail(f"trace/events.jsonl line {ln}: elapsed_ms must be number")

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

e2e_validate_bundle_tree() {
    local root_dir="${1:-${E2E_PROJECT_ROOT}/tests/artifacts}"
    if [ ! -d "$root_dir" ]; then
        e2e_log "No artifact root at ${root_dir}; skipping bundle-tree validation"
        return 0
    fi

    local found=0
    local failed=0
    while IFS= read -r manifest; do
        (( found++ )) || true
        local bundle_dir
        bundle_dir="$(dirname "$manifest")"
        if ! e2e_validate_bundle_manifest "$bundle_dir"; then
            e2e_log "Invalid bundle: ${bundle_dir}"
            (( failed++ )) || true
        fi
    done < <(find "$root_dir" -type f -name "bundle.json" | sort)

    if [ "$found" -eq 0 ]; then
        e2e_log "No bundle.json files found under ${root_dir}"
        return 0
    fi
    if [ "$failed" -gt 0 ]; then
        e2e_log "Bundle-tree validation failed: ${failed}/${found} invalid"
        return 1
    fi
    e2e_log "Bundle-tree validation passed: ${found} bundle(s)"
    return 0
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
# JSON-RPC Request/Response Capture (br-3h13.12.1)
# ---------------------------------------------------------------------------
#
# e2e_rpc_call: Composable JSON-RPC caller with full artifact capture.
#
# Usage:
#   e2e_rpc_call <case_id> <url> <tool_name> [arguments_json] [extra_headers...]
#
# Artifacts saved to ${E2E_ARTIFACT_DIR}/<case_id>/:
#   request.json   - Full JSON-RPC request body
#   response.json  - Full response body
#   headers.txt    - HTTP response headers
#   timing.txt     - Elapsed time in milliseconds
#   status.txt     - HTTP status code
#   curl_stderr.txt - curl stderr output (for debugging)
#   diagnostics.txt - Auto-saved on non-200 status or error
#
# Returns:
#   0 on success (HTTP 200), 1 otherwise
#   Response body is saved to ${E2E_ARTIFACT_DIR}/<case_id>/response.json
#
# Environment variables:
#   E2E_RPC_CALL_HOOK - Optional: path to script called after each request.
#       Receives: $1=case_id, $2=status_code, $3=elapsed_ms, $4=artifact_dir
#
# Example:
#   e2e_rpc_call "test_health_check" "http://127.0.0.1:8765/mcp/" "health_check"
#   e2e_rpc_call "test_ensure" "http://127.0.0.1:8765/mcp/" "ensure_project" '{"human_key":"/tmp/test"}'
#   e2e_rpc_call "test_auth" "http://127.0.0.1:8765/mcp/" "health_check" '{}' "Authorization: Bearer tok123"

e2e_rpc_call() {
    local case_id="$1"
    local url="$2"
    local tool_name="$3"
    local args_json="${4:-{\}}"
    shift 4 2>/dev/null || shift 3 2>/dev/null || true

    # Create case-specific artifact directory
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    mkdir -p "${case_dir}"

    local request_file="${case_dir}/request.json"
    local response_file="${case_dir}/response.json"
    local headers_file="${case_dir}/headers.txt"
    local timing_file="${case_dir}/timing.txt"
    local status_file="${case_dir}/status.txt"
    local curl_stderr_file="${case_dir}/curl_stderr.txt"
    local diagnostics_file="${case_dir}/diagnostics.txt"

    # Build JSON-RPC request payload
    local payload
    local connect_timeout="${E2E_RPC_CONNECT_TIMEOUT_SECONDS:-}"
    local max_time="${E2E_RPC_MAX_TIME_SECONDS:-}"
    payload="$(cat <<EOJSON
{"jsonrpc":"2.0","method":"tools/call","id":1,"params":{"name":"${tool_name}","arguments":${args_json}}}
EOJSON
)"
    echo "${payload}" > "${request_file}"

    # Build curl arguments
    local curl_args=(
        -sS
        -D "${headers_file}"
        -o "${response_file}"
        -w "%{http_code}|%{time_total}"
        -X POST
        "${url}"
        -H "content-type: application/json"
        --data "@${request_file}"
    )
    if [ -n "${connect_timeout}" ]; then
        curl_args+=(--connect-timeout "${connect_timeout}")
    fi
    if [ -n "${max_time}" ]; then
        curl_args+=(--max-time "${max_time}")
    fi

    # Add extra headers from remaining args
    for h in "$@"; do
        curl_args+=(-H "$h")
    done

    # Execute curl
    set +e
    local curl_output
    curl_output="$(curl "${curl_args[@]}" 2>"${curl_stderr_file}")"
    local curl_rc=$?
    set -e

    # Parse curl output (status|time_total)
    local http_status time_total elapsed_ms
    http_status="${curl_output%%|*}"
    time_total="${curl_output##*|}"
    # Convert time_total (seconds with decimals) to milliseconds
    if command -v python3 >/dev/null 2>&1; then
        elapsed_ms="$(python3 -c "print(int(float('${time_total}') * 1000))" 2>/dev/null || echo "0")"
    else
        elapsed_ms="$(echo "${time_total} * 1000 / 1" | bc 2>/dev/null || echo "0")"
    fi

    echo "${http_status}" > "${status_file}"
    echo "${elapsed_ms}" > "${timing_file}"

    # Handle curl failure
    if [ "$curl_rc" -ne 0 ]; then
        {
            echo "CURL_FAILURE"
            echo "curl_rc=${curl_rc}"
            echo "case_id=${case_id}"
            echo "url=${url}"
            echo "tool=${tool_name}"
            echo ""
            echo "=== curl stderr ==="
            cat "${curl_stderr_file}" 2>/dev/null || echo "(empty)"
            echo ""
            echo "=== request ==="
            cat "${request_file}"
        } > "${diagnostics_file}"
        _e2e_trace_event "rpc_call_fail" "curl_rc=${curl_rc}" "${case_id}"
        return 1
    fi

    # Handle non-200 status
    if [ "${http_status}" != "200" ]; then
        {
            echo "HTTP_ERROR"
            echo "status=${http_status}"
            echo "case_id=${case_id}"
            echo "url=${url}"
            echo "tool=${tool_name}"
            echo "elapsed_ms=${elapsed_ms}"
            echo ""
            echo "=== response headers ==="
            cat "${headers_file}" 2>/dev/null || echo "(no headers)"
            echo ""
            echo "=== response body ==="
            cat "${response_file}" 2>/dev/null || echo "(no body)"
            echo ""
            echo "=== request ==="
            cat "${request_file}"
        } > "${diagnostics_file}"
        _e2e_trace_event "rpc_call_fail" "status=${http_status}" "${case_id}"

        # Call hook if defined
        if [ -n "${E2E_RPC_CALL_HOOK:-}" ] && [ -x "${E2E_RPC_CALL_HOOK}" ]; then
            "${E2E_RPC_CALL_HOOK}" "${case_id}" "${http_status}" "${elapsed_ms}" "${case_dir}" || true
        fi

        return 1
    fi

    # Success - trace event and call hook
    _e2e_trace_event "rpc_call_ok" "status=200 elapsed_ms=${elapsed_ms}" "${case_id}"

    if [ -n "${E2E_RPC_CALL_HOOK:-}" ] && [ -x "${E2E_RPC_CALL_HOOK}" ]; then
        "${E2E_RPC_CALL_HOOK}" "${case_id}" "${http_status}" "${elapsed_ms}" "${case_dir}" || true
    fi

    return 0
}

# e2e_rpc_call_raw: Like e2e_rpc_call but with raw JSON-RPC payload (for non-tools/call)
#
# Usage:
#   e2e_rpc_call_raw <case_id> <url> <payload_json> [extra_headers...]

e2e_rpc_call_raw() {
    local case_id="$1"
    local url="$2"
    local payload="$3"
    shift 3 2>/dev/null || true

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local connect_timeout="${E2E_RPC_CONNECT_TIMEOUT_SECONDS:-}"
    local max_time="${E2E_RPC_MAX_TIME_SECONDS:-}"
    mkdir -p "${case_dir}"

    local request_file="${case_dir}/request.json"
    local response_file="${case_dir}/response.json"
    local headers_file="${case_dir}/headers.txt"
    local timing_file="${case_dir}/timing.txt"
    local status_file="${case_dir}/status.txt"
    local curl_stderr_file="${case_dir}/curl_stderr.txt"
    local diagnostics_file="${case_dir}/diagnostics.txt"

    echo "${payload}" > "${request_file}"

    local curl_args=(
        -sS
        -D "${headers_file}"
        -o "${response_file}"
        -w "%{http_code}|%{time_total}"
        -X POST
        "${url}"
        -H "content-type: application/json"
        --data "@${request_file}"
    )
    if [ -n "${connect_timeout}" ]; then
        curl_args+=(--connect-timeout "${connect_timeout}")
    fi
    if [ -n "${max_time}" ]; then
        curl_args+=(--max-time "${max_time}")
    fi

    for h in "$@"; do
        curl_args+=(-H "$h")
    done

    set +e
    local curl_output
    curl_output="$(curl "${curl_args[@]}" 2>"${curl_stderr_file}")"
    local curl_rc=$?
    set -e

    local http_status time_total elapsed_ms
    http_status="${curl_output%%|*}"
    time_total="${curl_output##*|}"
    if command -v python3 >/dev/null 2>&1; then
        elapsed_ms="$(python3 -c "print(int(float('${time_total}') * 1000))" 2>/dev/null || echo "0")"
    else
        elapsed_ms="$(echo "${time_total} * 1000 / 1" | bc 2>/dev/null || echo "0")"
    fi

    echo "${http_status}" > "${status_file}"
    echo "${elapsed_ms}" > "${timing_file}"

    if [ "$curl_rc" -ne 0 ]; then
        {
            echo "CURL_FAILURE"
            echo "curl_rc=${curl_rc}"
            echo "case_id=${case_id}"
            echo "url=${url}"
            echo ""
            echo "=== curl stderr ==="
            cat "${curl_stderr_file}" 2>/dev/null || echo "(empty)"
            echo ""
            echo "=== request ==="
            cat "${request_file}"
        } > "${diagnostics_file}"
        _e2e_trace_event "rpc_call_fail" "curl_rc=${curl_rc}" "${case_id}"
        return 1
    fi

    if [ "${http_status}" != "200" ]; then
        {
            echo "HTTP_ERROR"
            echo "status=${http_status}"
            echo "case_id=${case_id}"
            echo "url=${url}"
            echo "elapsed_ms=${elapsed_ms}"
            echo ""
            echo "=== response headers ==="
            cat "${headers_file}" 2>/dev/null || echo "(no headers)"
            echo ""
            echo "=== response body ==="
            cat "${response_file}" 2>/dev/null || echo "(no body)"
            echo ""
            echo "=== request ==="
            cat "${request_file}"
        } > "${diagnostics_file}"
        _e2e_trace_event "rpc_call_fail" "status=${http_status}" "${case_id}"

        if [ -n "${E2E_RPC_CALL_HOOK:-}" ] && [ -x "${E2E_RPC_CALL_HOOK}" ]; then
            "${E2E_RPC_CALL_HOOK}" "${case_id}" "${http_status}" "${elapsed_ms}" "${case_dir}" || true
        fi

        return 1
    fi

    _e2e_trace_event "rpc_call_ok" "status=200 elapsed_ms=${elapsed_ms}" "${case_id}"

    if [ -n "${E2E_RPC_CALL_HOOK:-}" ] && [ -x "${E2E_RPC_CALL_HOOK}" ]; then
        "${E2E_RPC_CALL_HOOK}" "${case_id}" "${http_status}" "${elapsed_ms}" "${case_dir}" || true
    fi

    return 0
}

# e2e_rpc_read_response: Helper to read and parse response.json from a case
#
# Usage:
#   local body
#   body="$(e2e_rpc_read_response "test_case")"

e2e_rpc_read_response() {
    local case_id="$1"
    cat "${E2E_ARTIFACT_DIR}/${case_id}/response.json" 2>/dev/null || echo ""
}

# e2e_rpc_read_status: Helper to read HTTP status code from a case
#
# Usage:
#   local status
#   status="$(e2e_rpc_read_status "test_case")"

e2e_rpc_read_status() {
    local case_id="$1"
    cat "${E2E_ARTIFACT_DIR}/${case_id}/status.txt" 2>/dev/null || echo ""
}

# e2e_rpc_read_timing: Helper to read elapsed time in ms from a case
#
# Usage:
#   local ms
#   ms="$(e2e_rpc_read_timing "test_case")"

e2e_rpc_read_timing() {
    local case_id="$1"
    cat "${E2E_ARTIFACT_DIR}/${case_id}/timing.txt" 2>/dev/null || echo "0"
}

# e2e_rpc_assert_success: Assert an RPC call succeeded (HTTP 200 + no JSON-RPC error)
#
# Usage:
#   e2e_rpc_assert_success "test_case" "health check succeeds"

e2e_rpc_assert_success() {
    local case_id="$1"
    local label="$2"
    local status response

    status="$(e2e_rpc_read_status "${case_id}")"
    if [ "${status}" != "200" ]; then
        e2e_fail "${label} (HTTP ${status})"
        return 1
    fi

    response="$(e2e_rpc_read_response "${case_id}")"
    if echo "${response}" | grep -q '"error"'; then
        e2e_fail "${label} (JSON-RPC error in response)"
        return 1
    fi

    e2e_pass "${label}"
    return 0
}

# e2e_rpc_assert_error: Assert an RPC call returned HTTP non-200 or JSON-RPC error
#
# Usage:
#   e2e_rpc_assert_error "test_case" "invalid auth fails" "401"

e2e_rpc_assert_error() {
    local case_id="$1"
    local label="$2"
    local expected_status="${3:-}"
    local status

    status="$(e2e_rpc_read_status "${case_id}")"
    if [ -n "${expected_status}" ] && [ "${status}" != "${expected_status}" ]; then
        e2e_fail "${label} (expected HTTP ${expected_status}, got ${status})"
        return 1
    fi

    if [ "${status}" = "200" ]; then
        local response
        response="$(e2e_rpc_read_response "${case_id}")"
        if ! echo "${response}" | grep -q '"error"'; then
            e2e_fail "${label} (expected error, got success)"
            return 1
        fi
    fi

    e2e_pass "${label}"
    return 0
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
# Server Log Capture (br-3h13.12.2)
# ---------------------------------------------------------------------------
#
# Provides server lifecycle management with automatic log capture for E2E tests.
# Server logs are captured to artifacts/logs/server.log, with per-case markers
# for easy extraction of relevant log segments when tests fail.
#
# Usage:
#   e2e_start_server_with_logs "/path/to/db" "/path/to/storage" "label" [extra_env...]
#   e2e_mark_case_start "test_case_name"
#   # ... run test case ...
#   e2e_mark_case_end "test_case_name"
#   e2e_stop_server
#
# On test failure, call e2e_extract_case_logs to get relevant server output.

# Server state tracking
_E2E_SERVER_PID=""
_E2E_SERVER_LOG=""
_E2E_SERVER_LABEL=""
_E2E_CASE_MARKERS=()
_E2E_CASE_LOG_LINE_COUNTS=()

# e2e_start_server_with_logs: Start mcp-agent-mail server with debug logging
#
# Args:
#   $1: db_path - Path to SQLite database file
#   $2: storage_root - Path to storage root directory
#   $3: label - Server label for log file naming
#   $4+: Extra env vars as KEY=VALUE pairs
#
# Example:
#   e2e_start_server_with_logs "/tmp/db.sqlite3" "/tmp/storage" "main" "HTTP_PORT=8765"

e2e_start_server_with_logs() {
    local db_path="$1"
    local storage_root="$2"
    local label="${3:-server}"
    shift 3 2>/dev/null || shift 2 2>/dev/null || true

    # Determine server binary
    local bin
    bin="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

    # Pick a random port if not specified
    local port="${HTTP_PORT:-}"
    if [ -z "$port" ]; then
        if command -v python3 >/dev/null 2>&1; then
            port="$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()')"
        else
            port="$((8000 + RANDOM % 1000))"
        fi
    fi

    # Server log file
    _E2E_SERVER_LOG="${E2E_ARTIFACT_DIR}/logs/server_${label}.log"
    _E2E_SERVER_LABEL="$label"
    mkdir -p "$(dirname "$_E2E_SERVER_LOG")"

    e2e_log "Starting server (${label}): 127.0.0.1:${port}"
    e2e_log "  log: $_E2E_SERVER_LOG"

    # Start server in background with debug logging
    (
        export DATABASE_URL="sqlite:////${db_path}"
        export STORAGE_ROOT="${storage_root}"
        export HTTP_HOST="127.0.0.1"
        export HTTP_PORT="${port}"
        export LOG_LEVEL="debug"
        export RUST_LOG="debug"

        # Apply extra env vars
        while [ $# -gt 0 ]; do
            export "$1"
            shift
        done

        "${bin}" serve --host 127.0.0.1 --port "${port}"
    ) >"$_E2E_SERVER_LOG" 2>&1 &
    _E2E_SERVER_PID=$!

    # Wait for server to be ready
    if ! e2e_wait_port "127.0.0.1" "${port}" 15; then
        e2e_log "Server failed to start within 15s"
        _e2e_server_startup_diagnostics
        return 1
    fi

    # Write server startup marker
    _e2e_server_log_marker "SERVER_STARTED" "pid=${_E2E_SERVER_PID} port=${port} label=${label}"

    # Export server URL for tests
    export E2E_SERVER_URL="http://127.0.0.1:${port}/mcp/"
    export E2E_SERVER_PID="${_E2E_SERVER_PID}"

    e2e_log "Server started: pid=${_E2E_SERVER_PID}"
    return 0
}

# e2e_stop_server: Stop the running server and finalize logs
e2e_stop_server() {
    if [ -z "${_E2E_SERVER_PID}" ]; then
        return 0
    fi

    _e2e_server_log_marker "SERVER_STOPPING" "pid=${_E2E_SERVER_PID}"

    if kill -0 "${_E2E_SERVER_PID}" 2>/dev/null; then
        # Graceful shutdown
        kill "${_E2E_SERVER_PID}" 2>/dev/null || true
        sleep 0.5

        # Force kill if still running
        if kill -0 "${_E2E_SERVER_PID}" 2>/dev/null; then
            kill -9 "${_E2E_SERVER_PID}" 2>/dev/null || true
        fi
    fi

    _e2e_server_log_marker "SERVER_STOPPED" "pid=${_E2E_SERVER_PID}"

    # Write server log stats
    if [ -f "$_E2E_SERVER_LOG" ]; then
        local line_count
        line_count="$(wc -l < "$_E2E_SERVER_LOG" 2>/dev/null || echo "0")"
        e2e_log "Server log: ${line_count} lines"
    fi

    _E2E_SERVER_PID=""
    unset E2E_SERVER_URL E2E_SERVER_PID
}

# e2e_mark_case_start: Write marker to server log at case start
#
# Usage:
#   e2e_mark_case_start "test_case_name"

e2e_mark_case_start() {
    local case_name="$1"
    if [ -z "$case_name" ]; then
        return 0
    fi

    # Idempotent for the currently active case (supports manual + auto calls).
    if [ "${_E2E_MARKER_ACTIVE_CASE:-}" = "$case_name" ]; then
        return 0
    fi

    # If another case is still active, close it first.
    if [ -n "${_E2E_MARKER_ACTIVE_CASE:-}" ] && [ "${_E2E_MARKER_ACTIVE_CASE}" != "$case_name" ]; then
        e2e_mark_case_end "${_E2E_MARKER_ACTIVE_CASE}"
    fi

    local marker="E2E_CASE_START:${case_name}:$(_e2e_now_rfc3339)"

    _E2E_CASE_MARKERS+=("${marker}")

    if [ -n "${_E2E_SERVER_LOG}" ] && [ -f "${_E2E_SERVER_LOG}" ]; then
        local line_num
        line_num="$(wc -l < "$_E2E_SERVER_LOG" 2>/dev/null || echo "0")"
        _E2E_CASE_LOG_LINE_COUNTS+=("${case_name}:${line_num}")
    fi

    _E2E_MARKER_ACTIVE_CASE="$case_name"
    _e2e_server_log_marker "CASE_START" "$case_name"
}

# e2e_mark_case_end: Write marker to server log at case end
#
# Usage:
#   e2e_mark_case_end "test_case_name"

e2e_mark_case_end() {
    local case_name="$1"
    if [ -z "$case_name" ]; then
        return 0
    fi
    _e2e_server_log_marker "CASE_END" "$case_name"
    if [ "${_E2E_MARKER_ACTIVE_CASE:-}" = "$case_name" ]; then
        _E2E_MARKER_ACTIVE_CASE=""
    fi
}

# e2e_extract_case_logs: Extract server logs for a specific test case
#
# Usage:
#   e2e_extract_case_logs "test_case_name"
#
# Output:
#   Saves logs to ${E2E_ARTIFACT_DIR}/${case_name}/server_logs.txt
#   Returns the extracted log content

e2e_extract_case_logs() {
    local case_name="$1"
    local out_file="${E2E_ARTIFACT_DIR}/${case_name}/server_logs.txt"

    if [ -z "${_E2E_SERVER_LOG}" ] || [ ! -f "${_E2E_SERVER_LOG}" ]; then
        echo "(no server log available)"
        return 0
    fi

    mkdir -p "$(dirname "$out_file")"

    # Find start and end line numbers for this case
    local start_line=0 end_line
    local entry
    for entry in "${_E2E_CASE_LOG_LINE_COUNTS[@]}"; do
        local entry_case="${entry%%:*}"
        local entry_line="${entry##*:}"
        if [ "$entry_case" = "$case_name" ]; then
            start_line="$entry_line"
            break
        fi
    done

    # Get current line count as end
    end_line="$(wc -l < "$_E2E_SERVER_LOG" 2>/dev/null || echo "0")"

    # Extract lines between markers
    if [ "$start_line" -gt 0 ]; then
        sed -n "${start_line},${end_line}p" "$_E2E_SERVER_LOG" > "$out_file"
    else
        # Fallback: grep for case name in logs
        grep -i "$case_name" "$_E2E_SERVER_LOG" > "$out_file" 2>/dev/null || true
    fi

    cat "$out_file"
}

# e2e_get_server_logs_tail: Get the last N lines of server log
#
# Usage:
#   e2e_get_server_logs_tail 50

e2e_get_server_logs_tail() {
    local n="${1:-50}"
    if [ -n "${_E2E_SERVER_LOG}" ] && [ -f "${_E2E_SERVER_LOG}" ]; then
        tail -n "$n" "$_E2E_SERVER_LOG"
    fi
}

# Internal: Write marker to server log (via echo to log file)
_e2e_server_log_marker() {
    local kind="$1"
    local msg="${2:-}"
    local ts
    ts="$(_e2e_now_rfc3339)"

    if [ -n "${_E2E_SERVER_LOG}" ]; then
        echo "[E2E_MARKER] ${ts} ${kind}: ${msg}" >> "$_E2E_SERVER_LOG" 2>/dev/null || true
    fi
}

# Internal: Diagnostics on server startup failure
_e2e_server_startup_diagnostics() {
    local diag_file="${E2E_ARTIFACT_DIR}/diagnostics/server_startup_failure.txt"
    mkdir -p "$(dirname "$diag_file")"

    {
        echo "Server startup failure diagnostics"
        echo "=================================="
        echo "Timestamp: $(_e2e_now_rfc3339)"
        echo "Label: ${_E2E_SERVER_LABEL}"
        echo "PID: ${_E2E_SERVER_PID}"
        echo ""
        echo "=== Server log (last 100 lines) ==="
        if [ -n "${_E2E_SERVER_LOG}" ] && [ -f "${_E2E_SERVER_LOG}" ]; then
            tail -n 100 "$_E2E_SERVER_LOG"
        else
            echo "(no log file)"
        fi
        echo ""
        echo "=== Process status ==="
        ps aux | grep -E "mcp-agent-mail|$_E2E_SERVER_PID" | grep -v grep || echo "(no matching processes)"
        echo ""
        echo "=== Port status ==="
        ss -tlnp 2>/dev/null | head -20 || netstat -tlnp 2>/dev/null | head -20 || echo "(unable to check ports)"
    } > "$diag_file"

    e2e_log "Startup diagnostics saved to: $diag_file"
}

# e2e_write_server_log_stats: Include server log stats in summary JSON
#
# Called automatically by e2e_summary if server was started
e2e_write_server_log_stats() {
    local artifact_dir="${1:-$E2E_ARTIFACT_DIR}"
    local stats_file="${artifact_dir}/logs/server_stats.json"

    if [ -z "${_E2E_SERVER_LOG}" ]; then
        return 0
    fi

    mkdir -p "$(dirname "$stats_file")"

    local line_count=0 error_count=0 warn_count=0
    if [ -f "${_E2E_SERVER_LOG}" ]; then
        line_count="$(wc -l < "$_E2E_SERVER_LOG" 2>/dev/null || echo "0")"
        error_count="$(grep -ci 'error\|ERROR' "$_E2E_SERVER_LOG" 2>/dev/null || true)"
        warn_count="$(grep -ci 'warn\|WARN' "$_E2E_SERVER_LOG" 2>/dev/null || true)"
        if [ -z "${error_count}" ]; then
            error_count="0"
        fi
        if [ -z "${warn_count}" ]; then
            warn_count="0"
        fi
    fi

    # Build per-case line counts JSON
    local case_counts_json="["
    local first=1
    local entry
    for entry in "${_E2E_CASE_LOG_LINE_COUNTS[@]}"; do
        local case_name="${entry%%:*}"
        local start_line="${entry##*:}"
        if [ "$first" -eq 1 ]; then
            first=0
        else
            case_counts_json="${case_counts_json},"
        fi
        case_counts_json="${case_counts_json}{\"case\":\"$(_e2e_json_escape "$case_name")\",\"start_line\":${start_line}}"
    done
    case_counts_json="${case_counts_json}]"

    cat > "$stats_file" <<EOJSON
{
  "schema_version": 1,
  "suite": "$(_e2e_json_escape "$E2E_SUITE")",
  "timestamp": "$(_e2e_json_escape "$E2E_TIMESTAMP")",
  "server_label": "$(_e2e_json_escape "${_E2E_SERVER_LABEL:-}")",
  "log_file": "$(_e2e_json_escape "${_E2E_SERVER_LOG:-}")",
  "stats": {
    "total_lines": ${line_count},
    "error_lines": ${error_count},
    "warn_lines": ${warn_count}
  },
  "case_markers": ${case_counts_json}
}
EOJSON
}

# Enhanced e2e_fail to auto-extract server logs on failure
_e2e_original_fail() {
    local msg="${1:-}"
    (( _E2E_FAIL++ )) || true
    (( _E2E_ASSERT_SEQ++ )) || true
    local aid="${_E2E_CURRENT_CASE:+${_E2E_CURRENT_CASE}.}a${_E2E_ASSERT_SEQ}"
    local now_ms elapsed=""
    now_ms="$(_e2e_now_ms)"
    if [ "${_E2E_CASE_START_MS:-0}" -gt 0 ]; then
        elapsed=$(( now_ms - _E2E_CASE_START_MS ))
    fi
    _e2e_trace_event "assert_fail" "$msg" "" "$aid" "" "$elapsed"
    echo -e "  ${_e2e_color_red}FAIL${_e2e_color_reset} ${msg}"
}

# Override e2e_fail to capture server logs on failure (if server is running)
e2e_fail() {
    local msg="${1:-}"
    _e2e_original_fail "$msg"

    # Auto-extract server logs for current case on failure
    if [ -n "${_E2E_CURRENT_CASE}" ] && [ -n "${_E2E_SERVER_LOG}" ]; then
        local case_logs
        case_logs="$(e2e_extract_case_logs "${_E2E_CURRENT_CASE}" 2>/dev/null)"
        if [ -n "$case_logs" ]; then
            echo -e "    ${_e2e_color_dim}=== Server logs (last 20 lines) ===${_e2e_color_reset}" >&2
            echo "$case_logs" | tail -20 | sed 's/^/    /' >&2
        fi
    fi
}

# ---------------------------------------------------------------------------
# Database Assertion Helpers (br-3h13.12.3)
# ---------------------------------------------------------------------------
#
# Provides direct database assertions for E2E tests, enabling verification
# of DB state independent of API responses. Requires sqlite3 CLI.
#
# Usage:
#   e2e_assert_db_row_count "/path/to/db.sqlite" "messages" 5
#   e2e_assert_db_value "/path/to/db.sqlite" "SELECT name FROM agents WHERE id=1" "BrownHawk"
#   e2e_assert_db_contains "/path/to/db.sqlite" "agents" "name" "BrownHawk"
#   result=$(e2e_db_query "/path/to/db.sqlite" "SELECT COUNT(*) FROM messages")

# e2e_db_query: Execute SQL query and return result
#
# Args:
#   $1: db_path - Path to SQLite database file
#   $2: sql - SQL query to execute
#
# Returns:
#   Query result on stdout, exit code from sqlite3
#
# Example:
#   count=$(e2e_db_query "/tmp/db.sqlite" "SELECT COUNT(*) FROM messages")

e2e_db_query() {
    local db_path="$1"
    local sql="$2"

    if ! command -v sqlite3 >/dev/null 2>&1; then
        echo "(sqlite3 not found)"
        return 1
    fi

    if [ ! -f "$db_path" ]; then
        echo "(database not found: $db_path)"
        return 1
    fi

    sqlite3 -batch -noheader "$db_path" "$sql" 2>&1
}

# e2e_assert_db_row_count: Assert table has expected row count
#
# Args:
#   $1: db_path - Path to SQLite database file
#   $2: table - Table name
#   $3: expected_count - Expected row count
#   $4: label (optional) - Assertion label
#
# Example:
#   e2e_assert_db_row_count "/tmp/db.sqlite" "messages" 5 "messages table has 5 rows"

e2e_assert_db_row_count() {
    local db_path="$1"
    local table="$2"
    local expected_count="$3"
    local label="${4:-DB row count: ${table}}"

    if ! command -v sqlite3 >/dev/null 2>&1; then
        e2e_skip "${label} (sqlite3 not available)"
        return 0
    fi

    local actual_count
    actual_count="$(e2e_db_query "$db_path" "SELECT COUNT(*) FROM ${table}" 2>/dev/null | tr -d '[:space:]')"

    if [ "$actual_count" = "$expected_count" ]; then
        e2e_pass "${label} (${actual_count} rows)"
    else
        e2e_fail "${label}"
        e2e_diff "row count in ${table}" "$expected_count" "$actual_count"
    fi
}

# e2e_assert_db_value: Assert SQL query returns expected value
#
# Args:
#   $1: db_path - Path to SQLite database file
#   $2: sql - SQL query (should return single value)
#   $3: expected_value - Expected result
#   $4: label (optional) - Assertion label
#
# Example:
#   e2e_assert_db_value "/tmp/db.sqlite" "SELECT name FROM agents WHERE id=1" "BrownHawk"

e2e_assert_db_value() {
    local db_path="$1"
    local sql="$2"
    local expected_value="$3"
    local label="${4:-DB value assertion}"

    if ! command -v sqlite3 >/dev/null 2>&1; then
        e2e_skip "${label} (sqlite3 not available)"
        return 0
    fi

    local actual_value
    actual_value="$(e2e_db_query "$db_path" "$sql" 2>/dev/null | head -1 | tr -d '[:space:]')"

    if [ "$actual_value" = "$expected_value" ]; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
        e2e_diff "SQL result" "$expected_value" "$actual_value"
        echo -e "    ${_e2e_color_dim}query: ${sql}${_e2e_color_reset}"
    fi
}

# e2e_assert_db_contains: Assert table contains row with specific column value
#
# Args:
#   $1: db_path - Path to SQLite database file
#   $2: table - Table name
#   $3: column - Column name
#   $4: value - Expected value
#   $5: label (optional) - Assertion label
#
# Example:
#   e2e_assert_db_contains "/tmp/db.sqlite" "agents" "name" "BrownHawk"

e2e_assert_db_contains() {
    local db_path="$1"
    local table="$2"
    local column="$3"
    local value="$4"
    local label="${5:-DB contains: ${table}.${column}='${value}'}"

    if ! command -v sqlite3 >/dev/null 2>&1; then
        e2e_skip "${label} (sqlite3 not available)"
        return 0
    fi

    # Escape single quotes in value for SQL
    local escaped_value="${value//\'/\'\'}"
    local sql="SELECT COUNT(*) FROM ${table} WHERE ${column} = '${escaped_value}'"

    local count
    count="$(e2e_db_query "$db_path" "$sql" 2>/dev/null | tr -d '[:space:]')"

    if [ "$count" -gt 0 ] 2>/dev/null; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
        echo -e "    ${_e2e_color_dim}no rows found matching ${column}='${value}' in ${table}${_e2e_color_reset}"
    fi
}

# e2e_assert_db_not_contains: Assert table does NOT contain row with specific column value
#
# Args:
#   $1: db_path - Path to SQLite database file
#   $2: table - Table name
#   $3: column - Column name
#   $4: value - Value that should NOT exist
#   $5: label (optional) - Assertion label
#
# Example:
#   e2e_assert_db_not_contains "/tmp/db.sqlite" "agents" "name" "DeletedAgent"

e2e_assert_db_not_contains() {
    local db_path="$1"
    local table="$2"
    local column="$3"
    local value="$4"
    local label="${5:-DB not contains: ${table}.${column}='${value}'}"

    if ! command -v sqlite3 >/dev/null 2>&1; then
        e2e_skip "${label} (sqlite3 not available)"
        return 0
    fi

    local escaped_value="${value//\'/\'\'}"
    local sql="SELECT COUNT(*) FROM ${table} WHERE ${column} = '${escaped_value}'"

    local count
    count="$(e2e_db_query "$db_path" "$sql" 2>/dev/null | tr -d '[:space:]')"

    if [ "$count" = "0" ] 2>/dev/null; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
        echo -e "    ${_e2e_color_dim}found ${count} row(s) matching ${column}='${value}' in ${table}${_e2e_color_reset}"
    fi
}

# e2e_db_dump_table: Dump table contents for debugging
#
# Args:
#   $1: db_path - Path to SQLite database file
#   $2: table - Table name
#   $3: limit (optional) - Max rows to dump (default: 10)
#
# Returns:
#   Table contents on stdout
#
# Example:
#   e2e_db_dump_table "/tmp/db.sqlite" "messages" 5

e2e_db_dump_table() {
    local db_path="$1"
    local table="$2"
    local limit="${3:-10}"

    if ! command -v sqlite3 >/dev/null 2>&1; then
        echo "(sqlite3 not found)"
        return 1
    fi

    sqlite3 -batch -header -column "$db_path" "SELECT * FROM ${table} LIMIT ${limit}" 2>&1
}

# e2e_db_schema: Get table schema for debugging
#
# Args:
#   $1: db_path - Path to SQLite database file
#   $2: table - Table name
#
# Returns:
#   Table schema on stdout
#
# Example:
#   e2e_db_schema "/tmp/db.sqlite" "messages"

e2e_db_schema() {
    local db_path="$1"
    local table="$2"

    if ! command -v sqlite3 >/dev/null 2>&1; then
        echo "(sqlite3 not found)"
        return 1
    fi

    sqlite3 -batch "$db_path" ".schema ${table}" 2>&1
}

# e2e_save_db_snapshot: Save table contents to artifacts for forensics
#
# Args:
#   $1: db_path - Path to SQLite database file
#   $2: table - Table name
#   $3: artifact_name (optional) - Artifact filename (default: db_${table}.txt)
#
# Example:
#   e2e_save_db_snapshot "/tmp/db.sqlite" "messages"

e2e_save_db_snapshot() {
    local db_path="$1"
    local table="$2"
    local artifact_name="${3:-db_${table}.txt}"

    if ! command -v sqlite3 >/dev/null 2>&1; then
        e2e_save_artifact "$artifact_name" "(sqlite3 not found)"
        return 0
    fi

    local content
    content="$(sqlite3 -batch -header -column "$db_path" "SELECT * FROM ${table}" 2>&1)"
    e2e_save_artifact "$artifact_name" "$content"
}

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_summary() {
    if [ -n "${_E2E_MARKER_ACTIVE_CASE:-}" ]; then
        e2e_mark_case_end "${_E2E_MARKER_ACTIVE_CASE}"
    fi

    # Stop server before generating summary (ensures logs are complete)
    e2e_stop_server 2>/dev/null || true

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
        e2e_write_forensic_indexes

        # Write server log stats if server was used (br-3h13.12.2)
        e2e_write_server_log_stats

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
