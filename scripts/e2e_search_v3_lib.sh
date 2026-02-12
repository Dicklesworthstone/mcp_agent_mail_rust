#!/usr/bin/env bash
# e2e_search_v3_lib.sh - Search V3 E2E logging/artifact harness for deterministic diagnostics
#
# br-2tnl.7.7: Build Search V3 E2E logging/artifact harness for deterministic diagnostics
#
# This harness EXTENDS scripts/e2e_lib.sh with Search V3 specific capabilities:
#   - Standardized artifact directory layout under test_logs/search_v3/<timestamp>/
#   - Mode/filter parameter logging (SearchMode, DocKind, ImportanceFilter, etc.)
#   - Ranking score diffs with expected vs actual comparison
#   - Index freshness timestamps and consistency tracking
#   - Semantic model version tracking for hybrid search
#   - Human-readable logs AND machine-readable JSON summaries
#   - Helper utilities to avoid duplicated boilerplate across Search V3 E2E scripts
#
# Usage:
#   E2E_SUITE="search_v3_stdio"
#   SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
#   source "${SCRIPT_DIR}/../../scripts/e2e_search_v3_lib.sh"
#
# Provides (in addition to e2e_lib.sh helpers):
#   - search_v3_init               Initialize Search V3 artifact layout
#   - search_v3_rpc_call           Like e2e_rpc_call but with search-specific logging
#   - search_v3_capture_params     Log mode/filter parameters for a test case
#   - search_v3_capture_ranking    Log ranking scores for diff comparison
#   - search_v3_capture_index_meta Log index freshness and consistency
#   - search_v3_capture_model_ver  Log semantic/embedding model version
#   - search_v3_assert_ranking     Assert ranking matches expected order
#   - search_v3_case_summary       Emit per-case JSON summary
#   - search_v3_suite_summary      Emit full suite JSON summary at end
#   - search_v3_fail_diff          Log failure with structured diff

set -euo pipefail

# ---------------------------------------------------------------------------
# Source base E2E library
# ---------------------------------------------------------------------------
# Get the directory where this script lives (handles being sourced from anywhere)
_SEARCH_V3_LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# SCRIPT_DIR may be set by the calling test script; if not, use this script's dir
SCRIPT_DIR="${SCRIPT_DIR:-${_SEARCH_V3_LIB_DIR}}"

# shellcheck source=e2e_lib.sh
source "${_SEARCH_V3_LIB_DIR}/e2e_lib.sh"

# ---------------------------------------------------------------------------
# Search V3 Configuration
# ---------------------------------------------------------------------------

# Search V3 artifact subdirectory (under test_logs/search_v3/<timestamp>/)
SEARCH_V3_LOG_ROOT="${E2E_PROJECT_ROOT}/test_logs/search_v3"
SEARCH_V3_RUN_DIR=""

# Search V3 modes
declare -A SEARCH_V3_MODES=(
    ["lexical"]="Full-text lexical search (Tantivy/FTS5)"
    ["semantic"]="Vector similarity search (embeddings)"
    ["hybrid"]="Two-tier fusion: lexical + semantic reranking"
    ["auto"]="Engine auto-selects based on query"
)

# DocKind types
declare -A SEARCH_V3_DOC_KINDS=(
    ["message"]="Message (subject + body)"
    ["agent"]="Agent profile"
    ["project"]="Project metadata"
    ["thread"]="Thread (aggregated messages)"
)

# Counters for summary
SEARCH_V3_CASE_COUNT=0
SEARCH_V3_PASS_COUNT=0
SEARCH_V3_FAIL_COUNT=0
SEARCH_V3_RANKING_TESTS=0
SEARCH_V3_RANKING_PASS=0

# Case result accumulator for JSON summary
SEARCH_V3_CASE_RESULTS=()

# Index metadata captured during run
SEARCH_V3_INDEX_LAST_REFRESH=""
SEARCH_V3_INDEX_DOC_COUNT=0
SEARCH_V3_SEMANTIC_MODEL=""
SEARCH_V3_SEMANTIC_MODEL_VERSION=""

# ---------------------------------------------------------------------------
# Initialization
# ---------------------------------------------------------------------------

# search_v3_init: Initialize Search V3 artifact directory layout
#
# Creates:
#   test_logs/search_v3/<timestamp>/
#     ├── cases/           Per-case artifacts
#     ├── rankings/        Ranking diff artifacts
#     ├── index_meta/      Index freshness snapshots
#     ├── summaries/       JSON summaries (case + suite level)
#     ├── logs/            Human-readable logs
#     └── run_manifest.json
search_v3_init() {
    local run_timestamp="${1:-${E2E_TIMESTAMP}}"

    SEARCH_V3_RUN_DIR="${SEARCH_V3_LOG_ROOT}/${run_timestamp}"

    mkdir -p "${SEARCH_V3_RUN_DIR}/cases"
    mkdir -p "${SEARCH_V3_RUN_DIR}/rankings"
    mkdir -p "${SEARCH_V3_RUN_DIR}/index_meta"
    mkdir -p "${SEARCH_V3_RUN_DIR}/summaries"
    mkdir -p "${SEARCH_V3_RUN_DIR}/logs"

    # Create run manifest
    local manifest_file="${SEARCH_V3_RUN_DIR}/run_manifest.json"
    cat > "${manifest_file}" << EOJSON
{
    "harness_version": "1.0.0",
    "run_timestamp": "${run_timestamp}",
    "run_started_at": "${E2E_RUN_STARTED_AT}",
    "suite_name": "${E2E_SUITE}",
    "clock_mode": "${E2E_CLOCK_MODE}",
    "seed": "${E2E_SEED}",
    "artifact_layout": {
        "cases": "Per-case request/response/timing artifacts",
        "rankings": "Ranking diff artifacts (expected vs actual)",
        "index_meta": "Index freshness and consistency snapshots",
        "summaries": "JSON summaries (case + suite level)",
        "logs": "Human-readable logs"
    }
}
EOJSON

    e2e_log "Search V3 harness initialized: ${SEARCH_V3_RUN_DIR}"
}

# ---------------------------------------------------------------------------
# Search-Specific RPC Helper
# ---------------------------------------------------------------------------

# search_v3_rpc_call: Like e2e_rpc_call but with search-specific parameter logging
#
# Usage:
#   search_v3_rpc_call <case_id> <url> <tool_name> <args_json> \
#       [--mode <mode>] [--doc-kind <kind>] [--filter-importance <imp>] \
#       [--expected-count <n>] [--expected-first <subject>] [extra_headers...]
#
# Captures additional artifacts:
#   - search_params.json: Mode, filters, pagination
#   - expected.json: Expected results for diff comparison
search_v3_rpc_call() {
    local case_id="$1"
    local url="$2"
    local tool_name="$3"
    local args_json="${4:-{}}"
    shift 4 2>/dev/null || shift 3 2>/dev/null || true

    # Parse optional search-specific flags
    local search_mode="auto"
    local doc_kind=""
    local filter_importance=""
    local expected_count=""
    local expected_first=""
    local extra_headers=()

    while [ $# -gt 0 ]; do
        case "$1" in
            --mode)
                search_mode="$2"
                shift 2
                ;;
            --doc-kind)
                doc_kind="$2"
                shift 2
                ;;
            --filter-importance)
                filter_importance="$2"
                shift 2
                ;;
            --expected-count)
                expected_count="$2"
                shift 2
                ;;
            --expected-first)
                expected_first="$2"
                shift 2
                ;;
            *)
                extra_headers+=("$1")
                shift
                ;;
        esac
    done

    # Create case-specific directory
    local case_dir="${SEARCH_V3_RUN_DIR}/cases/${case_id}"
    mkdir -p "${case_dir}"

    # Log search parameters
    cat > "${case_dir}/search_params.json" << EOJSON
{
    "case_id": "${case_id}",
    "tool_name": "${tool_name}",
    "search_mode": "${search_mode}",
    "doc_kind": "${doc_kind}",
    "filter_importance": "${filter_importance}",
    "timestamp": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
}
EOJSON

    # Log expected results if provided
    if [ -n "${expected_count}" ] || [ -n "${expected_first}" ]; then
        cat > "${case_dir}/expected.json" << EOJSON
{
    "expected_count": ${expected_count:-null},
    "expected_first_subject": ${expected_first:+\"$expected_first\"}${expected_first:-null}
}
EOJSON
    fi

    # Call base e2e_rpc_call (sets E2E_ARTIFACT_DIR to our case dir temporarily)
    local original_artifact_dir="${E2E_ARTIFACT_DIR}"
    export E2E_ARTIFACT_DIR="${case_dir}"

    local rpc_result=0
    e2e_rpc_call "${case_id}" "${url}" "${tool_name}" "${args_json}" "${extra_headers[@]}" || rpc_result=$?

    export E2E_ARTIFACT_DIR="${original_artifact_dir}"

    # Parse response for search-specific metrics
    if [ -f "${case_dir}/response.json" ] && [ "$rpc_result" -eq 0 ]; then
        _search_v3_extract_metrics "${case_id}" "${case_dir}"
    fi

    return $rpc_result
}

# Internal: Extract search-specific metrics from response
_search_v3_extract_metrics() {
    local case_id="$1"
    local case_dir="$2"

    local response_file="${case_dir}/response.json"
    [ -f "$response_file" ] || return 0

    # Extract metrics using python (jq-like parsing)
    python3 << EOPY > "${case_dir}/metrics.json" 2>/dev/null || echo '{}' > "${case_dir}/metrics.json"
import json
import sys

try:
    with open('${response_file}', 'r') as f:
        data = json.load(f)

    # Navigate to result.content[0].text and parse it
    content = data.get('result', {}).get('content', [])
    if content:
        text = content[0].get('text', '{}')
        result = json.loads(text)
    else:
        result = {}

    # Extract search metrics
    metrics = {
        "case_id": "${case_id}",
        "result_count": len(result.get('result', result if isinstance(result, list) else [])),
        "elapsed_ms": data.get('result', {}).get('meta', {}).get('elapsed_ms', 0),
        "mode_used": result.get('mode_used', result.get('explain', {}).get('mode_used', 'unknown')),
        "has_explain": 'explain' in result,
        "extracted_at": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
    }

    print(json.dumps(metrics, indent=2))
except Exception as e:
    print(json.dumps({"error": str(e), "case_id": "${case_id}"}))
EOPY
}

# ---------------------------------------------------------------------------
# Parameter Capture Helpers
# ---------------------------------------------------------------------------

# search_v3_capture_params: Explicitly log mode/filter parameters for a test case
#
# Usage:
#   search_v3_capture_params <case_id> \
#       --mode <mode> --doc-kind <kind> --importance <imp> \
#       --query <query> --limit <n> --offset <n> --project <key>
search_v3_capture_params() {
    local case_id="$1"
    shift

    local mode="" doc_kind="" importance="" query="" limit="" offset="" project=""

    while [ $# -gt 0 ]; do
        case "$1" in
            --mode) mode="$2"; shift 2 ;;
            --doc-kind) doc_kind="$2"; shift 2 ;;
            --importance) importance="$2"; shift 2 ;;
            --query) query="$2"; shift 2 ;;
            --limit) limit="$2"; shift 2 ;;
            --offset) offset="$2"; shift 2 ;;
            --project) project="$2"; shift 2 ;;
            *) shift ;;
        esac
    done

    local case_dir="${SEARCH_V3_RUN_DIR}/cases/${case_id}"
    mkdir -p "${case_dir}"

    cat > "${case_dir}/params_captured.json" << EOJSON
{
    "case_id": "${case_id}",
    "search_mode": "${mode}",
    "doc_kind": "${doc_kind}",
    "importance_filter": "${importance}",
    "query": "${query}",
    "limit": ${limit:-null},
    "offset": ${offset:-null},
    "project_key": "${project}",
    "captured_at": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
}
EOJSON

    # Human-readable log
    echo "[${case_id}] params: mode=${mode} doc_kind=${doc_kind} importance=${importance}" \
        >> "${SEARCH_V3_RUN_DIR}/logs/params.log"
}

# ---------------------------------------------------------------------------
# Ranking Capture and Assertion
# ---------------------------------------------------------------------------

# search_v3_capture_ranking: Capture ranking scores for diff comparison
#
# Usage:
#   search_v3_capture_ranking <case_id> <ranking_json>
#
# ranking_json format: [{"id": "...", "score": 0.95, "subject": "..."}, ...]
search_v3_capture_ranking() {
    local case_id="$1"
    local ranking_json="$2"

    local ranking_file="${SEARCH_V3_RUN_DIR}/rankings/${case_id}.json"

    cat > "${ranking_file}" << EOJSON
{
    "case_id": "${case_id}",
    "captured_at": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')",
    "ranking": ${ranking_json}
}
EOJSON
}

# search_v3_assert_ranking: Assert ranking matches expected order
#
# Usage:
#   search_v3_assert_ranking <case_id> <expected_json> <actual_json>
#
# Returns 0 on match, 1 on mismatch (with diff logged)
search_v3_assert_ranking() {
    local case_id="$1"
    local expected_json="$2"
    local actual_json="$3"

    SEARCH_V3_RANKING_TESTS=$((SEARCH_V3_RANKING_TESTS + 1))

    local diff_file="${SEARCH_V3_RUN_DIR}/rankings/${case_id}_diff.json"

    python3 << EOPY > "${diff_file}"
import json

expected = json.loads('''${expected_json}''')
actual = json.loads('''${actual_json}''')

# Extract IDs for order comparison
expected_ids = [item.get('id', item.get('message_id', str(i))) for i, item in enumerate(expected)]
actual_ids = [item.get('id', item.get('message_id', str(i))) for i, item in enumerate(actual)]

match = expected_ids == actual_ids

diff = {
    "case_id": "${case_id}",
    "match": match,
    "expected_order": expected_ids,
    "actual_order": actual_ids,
    "expected_count": len(expected_ids),
    "actual_count": len(actual_ids),
    "position_diffs": []
}

# Find position differences
for i, exp_id in enumerate(expected_ids):
    if i < len(actual_ids):
        if actual_ids[i] != exp_id:
            diff["position_diffs"].append({
                "position": i,
                "expected": exp_id,
                "actual": actual_ids[i]
            })
    else:
        diff["position_diffs"].append({
            "position": i,
            "expected": exp_id,
            "actual": None
        })

print(json.dumps(diff, indent=2))
EOPY

    # Check if match
    if grep -q '"match": true' "${diff_file}"; then
        SEARCH_V3_RANKING_PASS=$((SEARCH_V3_RANKING_PASS + 1))
        e2e_pass "[${case_id}] ranking order matches expected"
        return 0
    else
        e2e_fail "[${case_id}] ranking order mismatch (see ${diff_file})"
        # Log human-readable diff
        echo "[${case_id}] RANKING MISMATCH:" >> "${SEARCH_V3_RUN_DIR}/logs/ranking_diffs.log"
        cat "${diff_file}" >> "${SEARCH_V3_RUN_DIR}/logs/ranking_diffs.log"
        echo "" >> "${SEARCH_V3_RUN_DIR}/logs/ranking_diffs.log"
        return 1
    fi
}

# ---------------------------------------------------------------------------
# Index Metadata Capture
# ---------------------------------------------------------------------------

# search_v3_capture_index_meta: Log index freshness and consistency
#
# Usage:
#   search_v3_capture_index_meta <snapshot_id> --doc-count <n> --last-commit <ts> \
#       [--segments <n>] [--stale-count <n>] [--consistency <ok|stale|rebuilding>]
search_v3_capture_index_meta() {
    local snapshot_id="$1"
    shift

    local doc_count=0 last_commit="" segments="" stale_count="" consistency="unknown"

    while [ $# -gt 0 ]; do
        case "$1" in
            --doc-count) doc_count="$2"; shift 2 ;;
            --last-commit) last_commit="$2"; shift 2 ;;
            --segments) segments="$2"; shift 2 ;;
            --stale-count) stale_count="$2"; shift 2 ;;
            --consistency) consistency="$2"; shift 2 ;;
            *) shift ;;
        esac
    done

    local meta_file="${SEARCH_V3_RUN_DIR}/index_meta/${snapshot_id}.json"

    cat > "${meta_file}" << EOJSON
{
    "snapshot_id": "${snapshot_id}",
    "doc_count": ${doc_count},
    "last_commit": "${last_commit}",
    "segments": ${segments:-null},
    "stale_count": ${stale_count:-null},
    "consistency": "${consistency}",
    "captured_at": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
}
EOJSON

    # Update global index metadata
    SEARCH_V3_INDEX_LAST_REFRESH="${last_commit}"
    SEARCH_V3_INDEX_DOC_COUNT="${doc_count}"

    echo "[${snapshot_id}] index: ${doc_count} docs, last_commit=${last_commit}, consistency=${consistency}" \
        >> "${SEARCH_V3_RUN_DIR}/logs/index_meta.log"
}

# ---------------------------------------------------------------------------
# Semantic Model Version Tracking
# ---------------------------------------------------------------------------

# search_v3_capture_model_ver: Log semantic/embedding model version
#
# Usage:
#   search_v3_capture_model_ver --model <name> --version <ver> [--dims <n>] [--provider <name>]
search_v3_capture_model_ver() {
    local model="" version="" dims="" provider=""

    while [ $# -gt 0 ]; do
        case "$1" in
            --model) model="$2"; shift 2 ;;
            --version) version="$2"; shift 2 ;;
            --dims) dims="$2"; shift 2 ;;
            --provider) provider="$2"; shift 2 ;;
            *) shift ;;
        esac
    done

    SEARCH_V3_SEMANTIC_MODEL="${model}"
    SEARCH_V3_SEMANTIC_MODEL_VERSION="${version}"

    cat > "${SEARCH_V3_RUN_DIR}/index_meta/semantic_model.json" << EOJSON
{
    "model_name": "${model}",
    "model_version": "${version}",
    "embedding_dims": ${dims:-null},
    "provider": "${provider}",
    "captured_at": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
}
EOJSON

    echo "semantic_model: ${model} v${version} (${dims:-?} dims, provider=${provider})" \
        >> "${SEARCH_V3_RUN_DIR}/logs/models.log"
}

# ---------------------------------------------------------------------------
# Case and Suite Summaries
# ---------------------------------------------------------------------------

# search_v3_case_summary: Emit per-case JSON summary
#
# Usage:
#   search_v3_case_summary <case_id> <status> [--elapsed <ms>] [--message <msg>]
search_v3_case_summary() {
    local case_id="$1"
    local status="$2"
    shift 2

    local elapsed_ms=0 message=""

    while [ $# -gt 0 ]; do
        case "$1" in
            --elapsed) elapsed_ms="$2"; shift 2 ;;
            --message) message="$2"; shift 2 ;;
            *) shift ;;
        esac
    done

    SEARCH_V3_CASE_COUNT=$((SEARCH_V3_CASE_COUNT + 1))
    if [ "$status" = "pass" ]; then
        SEARCH_V3_PASS_COUNT=$((SEARCH_V3_PASS_COUNT + 1))
    else
        SEARCH_V3_FAIL_COUNT=$((SEARCH_V3_FAIL_COUNT + 1))
    fi

    local summary_file="${SEARCH_V3_RUN_DIR}/summaries/${case_id}.json"

    cat > "${summary_file}" << EOJSON
{
    "case_id": "${case_id}",
    "status": "${status}",
    "elapsed_ms": ${elapsed_ms},
    "message": "${message}",
    "completed_at": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
}
EOJSON

    # Accumulate for suite summary
    SEARCH_V3_CASE_RESULTS+=("${case_id}:${status}")
}

# search_v3_fail_diff: Log failure with structured diff
#
# Usage:
#   search_v3_fail_diff <case_id> --expected <exp> --actual <act> [--field <name>]
search_v3_fail_diff() {
    local case_id="$1"
    shift

    local expected="" actual="" field="value"

    while [ $# -gt 0 ]; do
        case "$1" in
            --expected) expected="$2"; shift 2 ;;
            --actual) actual="$2"; shift 2 ;;
            --field) field="$2"; shift 2 ;;
            *) shift ;;
        esac
    done

    local diff_file="${SEARCH_V3_RUN_DIR}/cases/${case_id}/failure_diff.json"
    mkdir -p "$(dirname "${diff_file}")"

    cat > "${diff_file}" << EOJSON
{
    "case_id": "${case_id}",
    "field": "${field}",
    "expected": "${expected}",
    "actual": "${actual}",
    "diff_at": "$(date -u '+%Y-%m-%dT%H:%M:%SZ')"
}
EOJSON

    e2e_fail "[${case_id}] ${field}: expected='${expected}' actual='${actual}'"

    echo "[${case_id}] DIFF ${field}: expected='${expected}' actual='${actual}'" \
        >> "${SEARCH_V3_RUN_DIR}/logs/failures.log"
}

# search_v3_suite_summary: Emit full suite JSON summary at end
#
# Usage:
#   search_v3_suite_summary
search_v3_suite_summary() {
    local run_ended_at
    run_ended_at="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"

    local summary_file="${SEARCH_V3_RUN_DIR}/summaries/suite_summary.json"

    # Calculate elapsed time
    local start_epoch="${E2E_RUN_START_EPOCH_S}"
    local end_epoch
    end_epoch="$(date +%s)"
    local elapsed_s=$((end_epoch - start_epoch))

    # Build case results array
    local case_results_json="["
    local first=true
    for result in "${SEARCH_V3_CASE_RESULTS[@]:-}"; do
        if [ -z "$result" ]; then continue; fi
        local cid="${result%%:*}"
        local cstatus="${result##*:}"
        if [ "$first" = true ]; then
            first=false
        else
            case_results_json+=","
        fi
        case_results_json+="{\"case_id\":\"${cid}\",\"status\":\"${cstatus}\"}"
    done
    case_results_json+="]"

    cat > "${summary_file}" << EOJSON
{
    "suite_name": "${E2E_SUITE}",
    "harness_version": "1.0.0",
    "run_timestamp": "${E2E_TIMESTAMP}",
    "run_started_at": "${E2E_RUN_STARTED_AT}",
    "run_ended_at": "${run_ended_at}",
    "elapsed_seconds": ${elapsed_s},
    "clock_mode": "${E2E_CLOCK_MODE}",
    "seed": "${E2E_SEED}",
    "totals": {
        "cases": ${SEARCH_V3_CASE_COUNT},
        "passed": ${SEARCH_V3_PASS_COUNT},
        "failed": ${SEARCH_V3_FAIL_COUNT},
        "ranking_tests": ${SEARCH_V3_RANKING_TESTS},
        "ranking_passed": ${SEARCH_V3_RANKING_PASS}
    },
    "index_metadata": {
        "last_refresh": "${SEARCH_V3_INDEX_LAST_REFRESH}",
        "doc_count": ${SEARCH_V3_INDEX_DOC_COUNT},
        "semantic_model": "${SEARCH_V3_SEMANTIC_MODEL}",
        "semantic_model_version": "${SEARCH_V3_SEMANTIC_MODEL_VERSION}"
    },
    "case_results": ${case_results_json},
    "artifact_dir": "${SEARCH_V3_RUN_DIR}"
}
EOJSON

    # Human-readable summary to console and log
    echo ""
    echo "========================================================================"
    echo " Search V3 E2E Suite Summary: ${E2E_SUITE}"
    echo "========================================================================"
    echo " Cases:   ${SEARCH_V3_CASE_COUNT} total, ${SEARCH_V3_PASS_COUNT} passed, ${SEARCH_V3_FAIL_COUNT} failed"
    echo " Ranking: ${SEARCH_V3_RANKING_TESTS} tests, ${SEARCH_V3_RANKING_PASS} passed"
    echo " Elapsed: ${elapsed_s}s"
    echo " Artifacts: ${SEARCH_V3_RUN_DIR}"
    echo "========================================================================"

    # Also write to log
    cat > "${SEARCH_V3_RUN_DIR}/logs/summary.log" << EOLOG
Search V3 E2E Suite Summary: ${E2E_SUITE}
============================================
Run timestamp: ${E2E_TIMESTAMP}
Started: ${E2E_RUN_STARTED_AT}
Ended: ${run_ended_at}
Elapsed: ${elapsed_s}s

Results:
  Cases: ${SEARCH_V3_CASE_COUNT} total
  Passed: ${SEARCH_V3_PASS_COUNT}
  Failed: ${SEARCH_V3_FAIL_COUNT}

Ranking Tests:
  Total: ${SEARCH_V3_RANKING_TESTS}
  Passed: ${SEARCH_V3_RANKING_PASS}

Index Metadata:
  Last refresh: ${SEARCH_V3_INDEX_LAST_REFRESH}
  Doc count: ${SEARCH_V3_INDEX_DOC_COUNT}
  Semantic model: ${SEARCH_V3_SEMANTIC_MODEL} v${SEARCH_V3_SEMANTIC_MODEL_VERSION}

Artifacts: ${SEARCH_V3_RUN_DIR}
EOLOG

    # Return exit code based on failures
    if [ "${SEARCH_V3_FAIL_COUNT}" -gt 0 ]; then
        return 1
    fi
    return 0
}

# ---------------------------------------------------------------------------
# Convenience Helpers
# ---------------------------------------------------------------------------

# search_v3_banner: Print a search-specific banner
search_v3_banner() {
    local title="$1"
    e2e_banner "Search V3: ${title}"
}

# search_v3_log: Log a search-specific message
search_v3_log() {
    local msg="$1"
    e2e_log "[Search V3] ${msg}"
    echo "[$(date -u '+%Y-%m-%dT%H:%M:%SZ')] ${msg}" >> "${SEARCH_V3_RUN_DIR}/logs/harness.log"
}

# search_v3_mode_description: Get human description for mode
search_v3_mode_description() {
    local mode="$1"
    echo "${SEARCH_V3_MODES[${mode}]:-Unknown mode: ${mode}}"
}

# search_v3_doc_kind_description: Get human description for doc kind
search_v3_doc_kind_description() {
    local kind="$1"
    echo "${SEARCH_V3_DOC_KINDS[${kind}]:-Unknown kind: ${kind}}"
}

# ---------------------------------------------------------------------------
# Export
# ---------------------------------------------------------------------------

export -f search_v3_init
export -f search_v3_rpc_call
export -f search_v3_capture_params
export -f search_v3_capture_ranking
export -f search_v3_assert_ranking
export -f search_v3_capture_index_meta
export -f search_v3_capture_model_ver
export -f search_v3_case_summary
export -f search_v3_fail_diff
export -f search_v3_suite_summary
export -f search_v3_banner
export -f search_v3_log
export -f search_v3_mode_description
export -f search_v3_doc_kind_description

export SEARCH_V3_LOG_ROOT
export SEARCH_V3_RUN_DIR
