#!/usr/bin/env bash
# e2e_search_v3.sh - Search V3 E2E logging/artifact harness (br-2tnl.7.7)
#
# Provides standardized logging, artifact capture, and diagnostics for
# all Search V3 E2E test scripts. Extends e2e_lib.sh with search-specific
# concerns: mode/filter logging, ranking diffs, explain payloads, index
# freshness tracking, and deterministic diagnostics.
#
# Usage:
#   E2E_SUITE="search_v3_stdio"
#   source scripts/e2e_lib.sh
#   source scripts/e2e_search_v3.sh
#   sv3_init_harness  # After e2e_init_artifacts
#
# Provides:
#   sv3_init_harness          - Initialize Search V3 artifact layout
#   sv3_search                - Execute search with full artifact capture
#   sv3_search_stdio          - Execute search via stdio transport
#   sv3_compare_rankings      - Compare expected vs actual rankings
#   sv3_assert_mode           - Assert search mode in response
#   sv3_assert_explain        - Assert explain payload present
#   sv3_write_summary         - Write human-readable + JSON summary
#   sv3_log_index_freshness   - Log index freshness timestamps
#   sv3_log_model_version     - Log semantic model version
#
# Environment variables:
#   SV3_ARTIFACT_ROOT         - Override artifact root (default: test_logs/search_v3)
#   SV3_MODE_OVERRIDE         - Force search mode for all queries
#   SV3_EXPLAIN_ALL           - Enable explain mode for all queries
#   SV3_GOLDEN_DIR            - Directory for golden ranking files
#   SV3_STDIO_FIXTURE_RESPONSE - Optional JSONL fixture for stdio response replay
#   SV3_STDIO_FIXTURE_ELAPSED_MS - Optional elapsed_ms override when using fixture replay
#
# Reference: docs/ADR-003-search-v3-architecture.md

set -euo pipefail

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

# Verify e2e_lib.sh was sourced first
if [ -z "${E2E_SUITE:-}" ] || ! declare -f e2e_log >/dev/null 2>&1; then
    echo "ERROR: e2e_search_v3.sh requires e2e_lib.sh to be sourced first" >&2
    exit 1
fi

# Search V3 artifact root (separate from standard E2E artifacts)
SV3_ARTIFACT_ROOT="${SV3_ARTIFACT_ROOT:-${E2E_PROJECT_ROOT}/test_logs/search_v3}"

# Current run's Search V3 artifact directory
SV3_RUN_DIR=""

# Mode override for all searches
SV3_MODE_OVERRIDE="${SV3_MODE_OVERRIDE:-}"

# Force explain mode on all searches
SV3_EXPLAIN_ALL="${SV3_EXPLAIN_ALL:-false}"

# Golden ranking directory for regression tests
SV3_GOLDEN_DIR="${SV3_GOLDEN_DIR:-${E2E_PROJECT_ROOT}/tests/golden/search_v3}"

# Search V3 counters
_SV3_SEARCH_COUNT=0
_SV3_RANKING_MATCH=0
_SV3_RANKING_MISMATCH=0
_SV3_MODE_COUNTS=()

# Search V3 timing aggregates (in milliseconds)
declare -A _SV3_MODE_TOTAL_MS
declare -A _SV3_MODE_SEARCH_COUNT

# Index freshness tracking
_SV3_INDEX_FRESHNESS_TS=""
_SV3_SEMANTIC_MODEL_VERSION=""

# ---------------------------------------------------------------------------
# Initialization
# ---------------------------------------------------------------------------

# sv3_init_harness: Initialize Search V3 artifact layout for this run
#
# Creates:
#   ${SV3_ARTIFACT_ROOT}/${E2E_SUITE}/${E2E_TIMESTAMP}/
#   ├── queries/          - Per-query artifacts
#   ├── rankings/         - Ranking diffs and comparisons
#   ├── explain/          - Explain payloads
#   ├── timing/           - Latency metrics
#   ├── index/            - Index freshness logs
#   ├── summary.json      - Machine-readable summary
#   └── summary.txt       - Human-readable summary
#
sv3_init_harness() {
    SV3_RUN_DIR="${SV3_ARTIFACT_ROOT}/${E2E_SUITE}/${E2E_TIMESTAMP}"
    mkdir -p "${SV3_RUN_DIR}"/{queries,rankings,explain,timing,index}

    # Initialize mode timing arrays
    _SV3_MODE_TOTAL_MS[lexical]=0
    _SV3_MODE_TOTAL_MS[semantic]=0
    _SV3_MODE_TOTAL_MS[hybrid]=0
    _SV3_MODE_TOTAL_MS[auto]=0
    _SV3_MODE_SEARCH_COUNT[lexical]=0
    _SV3_MODE_SEARCH_COUNT[semantic]=0
    _SV3_MODE_SEARCH_COUNT[hybrid]=0
    _SV3_MODE_SEARCH_COUNT[auto]=0

    # Write run metadata
    cat > "${SV3_RUN_DIR}/run_metadata.json" <<EOJSON
{
    "schema_version": 1,
    "harness": "e2e_search_v3.sh",
    "bead": "br-2tnl.7.7",
    "suite": "$(_e2e_json_escape "${E2E_SUITE}")",
    "timestamp": "$(_e2e_json_escape "${E2E_TIMESTAMP}")",
    "clock_mode": "$(_e2e_json_escape "${E2E_CLOCK_MODE}")",
    "seed": ${E2E_SEED:-0},
    "run_started_at": "$(_e2e_json_escape "${E2E_RUN_STARTED_AT}")",
    "mode_override": "$(_e2e_json_escape "${SV3_MODE_OVERRIDE}")",
    "explain_all": ${SV3_EXPLAIN_ALL}
}
EOJSON

    e2e_log "Search V3 harness initialized: ${SV3_RUN_DIR}"
    _e2e_trace_event "sv3_init" "root=${SV3_RUN_DIR}"
}

# ---------------------------------------------------------------------------
# Search execution with full artifact capture
# ---------------------------------------------------------------------------

# sv3_search: Execute a search via HTTP with full Search V3 artifact capture
#
# Usage:
#   sv3_search <case_id> <url> <project_key> <query> [mode] [filters_json] [extra_headers...]
#
# Arguments:
#   case_id      - Unique identifier for this search (e.g., "lexical_basic_01")
#   url          - MCP endpoint URL (e.g., "http://127.0.0.1:8765/mcp/")
#   project_key  - Project path or slug
#   query        - Search query string
#   mode         - Search mode: lexical|semantic|hybrid|auto (default: auto)
#   filters_json - Optional JSON object for filters (e.g., '{"sender":"BlueLake"}')
#   extra_headers - Additional curl headers
#
# Artifacts saved to ${SV3_RUN_DIR}/queries/<case_id>/:
#   request.json   - Full JSON-RPC request
#   response.json  - Full response body
#   params.json    - Extracted search parameters (mode, filters, explain)
#   ranking.json   - Extracted ranking (message_ids + scores)
#   explain.json   - Explain payload if present
#   timing.txt     - Elapsed time in milliseconds
#   status.txt     - HTTP status code
#
# Returns:
#   0 on success (HTTP 200 + valid response), 1 otherwise
#
sv3_search() {
    local case_id="$1"
    local url="$2"
    local project_key="$3"
    local query="$4"
    local mode="${5:-auto}"
    local filters_json="${6:-{}}"
    shift 6 2>/dev/null || shift 5 2>/dev/null || shift 4 2>/dev/null || true

    # Apply mode override if set
    if [ -n "${SV3_MODE_OVERRIDE}" ]; then
        mode="${SV3_MODE_OVERRIDE}"
    fi

    # Build explain flag
    local explain_flag="false"
    if [ "${SV3_EXPLAIN_ALL}" = "true" ] || [ "${SV3_EXPLAIN_ALL}" = "1" ]; then
        explain_flag="true"
    fi

    # Create case directory under Search V3 layout
    local sv3_case_dir="${SV3_RUN_DIR}/queries/${case_id}"
    mkdir -p "${sv3_case_dir}"

    # Build search arguments
    local args_json
    args_json=$(python3 -c "
import json, sys
args = {
    'project_key': '$(_e2e_json_escape "${project_key}")',
    'query': '$(_e2e_json_escape "${query}")',
    'mode': '$(_e2e_json_escape "${mode}")',
    'explain': ${explain_flag}
}
filters = json.loads('${filters_json}')
if filters:
    args['filters'] = filters
print(json.dumps(args))
" 2>/dev/null || echo "{}")

    # Save search parameters
    cat > "${sv3_case_dir}/params.json" <<EOJSON
{
    "case_id": "$(_e2e_json_escape "${case_id}")",
    "query": "$(_e2e_json_escape "${query}")",
    "mode": "$(_e2e_json_escape "${mode}")",
    "explain": ${explain_flag},
    "filters": ${filters_json},
    "project_key": "$(_e2e_json_escape "${project_key}")"
}
EOJSON

    # Use the generic e2e_rpc_call with a symlink for compatibility
    # But also save to our Search V3 layout
    (( _SV3_SEARCH_COUNT++ )) || true

    # Execute the search via RPC
    local rpc_case_id="sv3_${case_id}"
    if ! e2e_rpc_call "${rpc_case_id}" "${url}" "search_messages" "${args_json}" "$@"; then
        # Copy diagnostics to Search V3 layout
        cp -r "${E2E_ARTIFACT_DIR}/${rpc_case_id}"/* "${sv3_case_dir}/" 2>/dev/null || true
        _sv3_trace_search "${case_id}" "fail" "${mode}" "0"
        return 1
    fi

    # Copy artifacts to Search V3 layout
    cp -r "${E2E_ARTIFACT_DIR}/${rpc_case_id}"/* "${sv3_case_dir}/" 2>/dev/null || true

    # Extract timing
    local elapsed_ms
    elapsed_ms="$(cat "${sv3_case_dir}/timing.txt" 2>/dev/null || echo "0")"

    # Update mode timing aggregates
    local mode_lower
    mode_lower="$(echo "${mode}" | tr '[:upper:]' '[:lower:]')"
    _SV3_MODE_TOTAL_MS[${mode_lower}]=$(( ${_SV3_MODE_TOTAL_MS[${mode_lower}]:-0} + ${elapsed_ms:-0} ))
    _SV3_MODE_SEARCH_COUNT[${mode_lower}]=$(( ${_SV3_MODE_SEARCH_COUNT[${mode_lower}]:-0} + 1 ))

    # Extract ranking from response
    _sv3_extract_ranking "${sv3_case_dir}"

    # Extract explain payload if present
    _sv3_extract_explain "${sv3_case_dir}"

    _sv3_trace_search "${case_id}" "ok" "${mode}" "${elapsed_ms}"
    return 0
}

# sv3_search_stdio: Execute a search via stdio transport with artifact capture
#
# Usage:
#   sv3_search_stdio <case_id> <db_path> <project_key> <query> [mode] [filters_json]
#
# Similar to sv3_search but uses the stdio transport instead of HTTP.
# Useful for tests that don't want to start an HTTP server.
#
sv3_search_stdio() {
    local case_id="$1"
    local db_path="$2"
    local project_key="$3"
    local query="$4"
    local mode="${5:-auto}"
    local filters_json="${6:-{}}"

    # Apply mode override if set
    if [ -n "${SV3_MODE_OVERRIDE}" ]; then
        mode="${SV3_MODE_OVERRIDE}"
    fi

    local explain_flag="false"
    if [ "${SV3_EXPLAIN_ALL}" = "true" ] || [ "${SV3_EXPLAIN_ALL}" = "1" ]; then
        explain_flag="true"
    fi

    local sv3_case_dir="${SV3_RUN_DIR}/queries/${case_id}"
    mkdir -p "${sv3_case_dir}"

    # Save search parameters
    cat > "${sv3_case_dir}/params.json" <<EOJSON
{
    "case_id": "$(_e2e_json_escape "${case_id}")",
    "query": "$(_e2e_json_escape "${query}")",
    "mode": "$(_e2e_json_escape "${mode}")",
    "explain": ${explain_flag},
    "filters": ${filters_json},
    "project_key": "$(_e2e_json_escape "${project_key}")",
    "transport": "stdio"
}
EOJSON

    # Build the JSON-RPC request
    local init_req='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"sv3-harness","version":"1.0"}}}'
    local search_args
    search_args=$(python3 -c "
import json
args = {
    'project_key': '$(_e2e_json_escape "${project_key}")',
    'query': '$(_e2e_json_escape "${query}")',
    'mode': '$(_e2e_json_escape "${mode}")',
    'explain': ${explain_flag}
}
filters = json.loads('${filters_json}')
if filters:
    args['filters'] = filters
print(json.dumps(args))
" 2>/dev/null || echo "{}")

    local search_req
    search_req="{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"search_messages\",\"arguments\":${search_args}}}"

    # Save request
    echo "${search_req}" > "${sv3_case_dir}/request.json"

    (( _SV3_SEARCH_COUNT++ )) || true

    # Execute via stdio
    local start_time end_time elapsed_ms
    start_time=$(date +%s%3N 2>/dev/null || echo "0")

    local fixture_response="${SV3_STDIO_FIXTURE_RESPONSE:-}"
    if [ -n "${fixture_response}" ]; then
        if [ ! -f "${fixture_response}" ]; then
            e2e_log "Search V3 stdio fixture missing: ${fixture_response}"
            _sv3_trace_search "${case_id}" "fail" "${mode}" "0"
            return 1
        fi
        cp "${fixture_response}" "${sv3_case_dir}/response_raw.txt"
        : > "${sv3_case_dir}/stderr.txt"
        elapsed_ms="${SV3_STDIO_FIXTURE_ELAPSED_MS:-1}"
    else
        local srv_work
        srv_work="$(mktemp -d "${TMPDIR}/sv3_stdio.XXXXXX")"
        local fifo="${srv_work}/stdin_fifo"
        mkfifo "$fifo"

        DATABASE_URL="sqlite:////${db_path}" RUST_LOG=error \
            mcp-agent-mail < "$fifo" > "${sv3_case_dir}/response_raw.txt" 2>"${sv3_case_dir}/stderr.txt" &
        local srv_pid=$!

        sleep 0.3

        {
            echo "$init_req"
            sleep 0.2
            echo "$search_req"
            sleep 0.3
        } > "$fifo" &
        local write_pid=$!

        local timeout_s=15 elapsed=0
        while [ "$elapsed" -lt "$timeout_s" ]; do
            if ! kill -0 "$srv_pid" 2>/dev/null; then break; fi
            sleep 0.3
            elapsed=$((elapsed + 1))
        done

        wait "$write_pid" 2>/dev/null || true
        kill "$srv_pid" 2>/dev/null || true
        wait "$srv_pid" 2>/dev/null || true
        rm -rf "$srv_work"

        end_time=$(date +%s%3N 2>/dev/null || echo "0")
        elapsed_ms=$(( end_time - start_time ))
    fi
    echo "${elapsed_ms}" > "${sv3_case_dir}/timing.txt"

    # Extract search response from raw output
    python3 -c "
import sys, json
found = False
for line in open('${sv3_case_dir}/response_raw.txt'):
    line = line.strip()
    if not line:
        continue
    try:
        d = json.loads(line)
    except json.JSONDecodeError:
        continue
    if d.get('id') == 2:
        print(json.dumps(d, indent=2))
        found = True
        break
if not found:
    print('{}')
" > "${sv3_case_dir}/response.json" 2>/dev/null

    # Treat tool/transport errors as search failure for deterministic assertions.
    local is_tool_error
    is_tool_error="$(python3 -c "
import json
try:
    data = json.load(open('${sv3_case_dir}/response.json'))
except Exception:
    print('true')
    raise SystemExit(0)

if not isinstance(data, dict) or not data:
    print('true')
elif 'error' in data:
    print('true')
elif 'result' not in data:
    print('true')
else:
    result = data.get('result', {})
    print('true' if isinstance(result, dict) and result.get('isError') else 'false')
" 2>/dev/null)"
    if [ "${is_tool_error}" = "true" ]; then
        echo "500" > "${sv3_case_dir}/status.txt"
        _sv3_trace_search "${case_id}" "fail" "${mode}" "${elapsed_ms}"
        return 1
    fi
    echo "200" > "${sv3_case_dir}/status.txt"

    # Update mode timing aggregates
    local mode_lower
    mode_lower="$(echo "${mode}" | tr '[:upper:]' '[:lower:]')"
    _SV3_MODE_TOTAL_MS[${mode_lower}]=$(( ${_SV3_MODE_TOTAL_MS[${mode_lower}]:-0} + ${elapsed_ms:-0} ))
    _SV3_MODE_SEARCH_COUNT[${mode_lower}]=$(( ${_SV3_MODE_SEARCH_COUNT[${mode_lower}]:-0} + 1 ))

    # Extract ranking and explain
    _sv3_extract_ranking "${sv3_case_dir}"
    _sv3_extract_explain "${sv3_case_dir}"

    _sv3_trace_search "${case_id}" "ok" "${mode}" "${elapsed_ms}"
    return 0
}

# ---------------------------------------------------------------------------
# Ranking comparison and assertions
# ---------------------------------------------------------------------------

# sv3_compare_rankings: Compare expected vs actual rankings
#
# Usage:
#   sv3_compare_rankings <case_id> <expected_ids_comma_sep> [tolerance]
#
# Arguments:
#   case_id              - Case ID from sv3_search
#   expected_ids         - Comma-separated list of expected message IDs in order
#   tolerance            - Position tolerance for partial match (default: 0 = exact)
#
# Returns:
#   0 if rankings match within tolerance, 1 otherwise
#
# Artifacts saved:
#   ${SV3_RUN_DIR}/rankings/<case_id>_diff.json - Diff details
#
sv3_compare_rankings() {
    local case_id="$1"
    local expected_ids="$2"
    local tolerance="${3:-0}"

    local ranking_file="${SV3_RUN_DIR}/queries/${case_id}/ranking.json"
    local diff_file="${SV3_RUN_DIR}/rankings/${case_id}_diff.json"

    if [ ! -f "${ranking_file}" ]; then
        cat > "${diff_file}" <<EOJSON
{
    "case_id": "$(_e2e_json_escape "${case_id}")",
    "status": "error",
    "error": "ranking file not found",
    "expected": "$(_e2e_json_escape "${expected_ids}")",
    "actual": null
}
EOJSON
        return 1
    fi

    # Compare rankings using Python
    local result
    result=$(python3 -c "
import json, sys

def compare_rankings(expected_str, actual_json, tolerance):
    expected = [int(x.strip()) for x in expected_str.split(',') if x.strip()]
    actual_data = json.load(open(actual_json))
    actual = [r.get('id') or r.get('message_id') for r in actual_data.get('results', [])]

    matches = []
    mismatches = []

    for i, exp_id in enumerate(expected):
        if i < len(actual):
            act_id = actual[i]
            if exp_id == act_id:
                matches.append({'pos': i, 'id': exp_id, 'status': 'exact'})
            elif tolerance > 0:
                # Check if it's within tolerance positions
                found_pos = None
                for j, a in enumerate(actual):
                    if a == exp_id and abs(i - j) <= tolerance:
                        found_pos = j
                        break
                if found_pos is not None:
                    matches.append({'pos': i, 'id': exp_id, 'actual_pos': found_pos, 'status': 'within_tolerance'})
                else:
                    mismatches.append({'pos': i, 'expected': exp_id, 'actual': act_id})
            else:
                mismatches.append({'pos': i, 'expected': exp_id, 'actual': act_id})
        else:
            mismatches.append({'pos': i, 'expected': exp_id, 'actual': None})

    # Check for extra results
    if len(actual) > len(expected):
        for i in range(len(expected), len(actual)):
            mismatches.append({'pos': i, 'expected': None, 'actual': actual[i], 'note': 'extra'})

    return {
        'case_id': '${case_id}',
        'status': 'match' if not mismatches else 'mismatch',
        'expected_count': len(expected),
        'actual_count': len(actual),
        'tolerance': ${tolerance},
        'matches': matches,
        'mismatches': mismatches
    }

result = compare_rankings('${expected_ids}', '${ranking_file}', ${tolerance})
print(json.dumps(result, indent=2))
" 2>/dev/null)

    echo "${result}" > "${diff_file}"

    if echo "${result}" | grep -q '"status": "match"'; then
        (( _SV3_RANKING_MATCH++ )) || true
        return 0
    else
        (( _SV3_RANKING_MISMATCH++ )) || true
        return 1
    fi
}

# sv3_compare_to_golden: Compare ranking to golden file
#
# Usage:
#   sv3_compare_to_golden <case_id> [golden_name]
#
# If golden_name is not provided, uses case_id as the golden file name.
# Golden files are stored in ${SV3_GOLDEN_DIR}/<golden_name>.json
#
sv3_compare_to_golden() {
    local case_id="$1"
    local golden_name="${2:-${case_id}}"
    local golden_file="${SV3_GOLDEN_DIR}/${golden_name}.json"

    if [ ! -f "${golden_file}" ]; then
        e2e_skip "golden file not found: ${golden_file}"
        return 2
    fi

    local expected_ids
    expected_ids=$(python3 -c "
import json
data = json.load(open('${golden_file}'))
ids = [str(r.get('id') or r.get('message_id')) for r in data.get('results', [])]
print(','.join(ids))
" 2>/dev/null)

    sv3_compare_rankings "${case_id}" "${expected_ids}"
}

# sv3_assert_mode: Assert the effective search mode in response
#
# Usage:
#   sv3_assert_mode <case_id> <expected_mode> [label]
#
sv3_assert_mode() {
    local case_id="$1"
    local expected_mode="$2"
    local label="${3:-search mode is ${expected_mode}}"

    local response_file="${SV3_RUN_DIR}/queries/${case_id}/response.json"
    if [ ! -f "${response_file}" ]; then
        e2e_fail "${label} (no response file)"
        return 1
    fi

    local actual_mode
    actual_mode=$(python3 -c "
import json
try:
    data = json.load(open('${response_file}'))
    result = data.get('result', {})
    if 'content' in result:
        inner = json.loads(result['content'][0]['text'])
    else:
        inner = result
    print(inner.get('mode', inner.get('search_mode', '')))
except:
    print('')
" 2>/dev/null)

    if [ "${actual_mode}" = "${expected_mode}" ]; then
        e2e_pass "${label}"
        return 0
    else
        e2e_fail "${label} (got ${actual_mode})"
        return 1
    fi
}

# sv3_assert_explain: Assert explain payload is present and valid
#
# Usage:
#   sv3_assert_explain <case_id> [label]
#
sv3_assert_explain() {
    local case_id="$1"
    local label="${2:-explain payload present}"

    local explain_file="${SV3_RUN_DIR}/queries/${case_id}/explain.json"
    if [ ! -f "${explain_file}" ]; then
        e2e_fail "${label} (no explain file)"
        return 1
    fi

    local has_explain
    has_explain=$(python3 -c "
import json
try:
    data = json.load(open('${explain_file}'))
    if data and (data.get('scores') or data.get('ranking_factors') or data.get('explain')):
        print('true')
    else:
        print('false')
except:
    print('false')
" 2>/dev/null)

    if [ "${has_explain}" = "true" ]; then
        e2e_pass "${label}"
        return 0
    else
        e2e_fail "${label}"
        return 1
    fi
}

# ---------------------------------------------------------------------------
# Index and model tracking
# ---------------------------------------------------------------------------

# sv3_log_index_freshness: Log index freshness timestamp
#
# Usage:
#   sv3_log_index_freshness <url> [project_key]
#
# Queries the server for index freshness and logs it to artifacts.
#
sv3_log_index_freshness() {
    local url="$1"
    local project_key="${2:-}"

    local freshness_file="${SV3_RUN_DIR}/index/freshness.json"

    # Query health_check for index info
    local case_id="sv3_freshness_$(date +%s)"
    if e2e_rpc_call "${case_id}" "${url}" "health_check" '{}' >/dev/null 2>&1; then
        local response
        response="$(e2e_rpc_read_response "${case_id}")"

        python3 -c "
import json, sys
try:
    data = json.loads('''${response}''')
    result = data.get('result', {})
    if 'content' in result:
        inner = json.loads(result['content'][0]['text'])
    else:
        inner = result

    freshness = {
        'logged_at': '$(date -u +%Y-%m-%dT%H:%M:%SZ)',
        'index_freshness': inner.get('index', {}).get('freshness_ts'),
        'last_indexed_id': inner.get('index', {}).get('last_indexed_id'),
        'document_count': inner.get('index', {}).get('document_count')
    }
    print(json.dumps(freshness, indent=2))
except Exception as e:
    print(json.dumps({'error': str(e)}))
" > "${freshness_file}" 2>/dev/null

        _SV3_INDEX_FRESHNESS_TS="$(python3 -c "import json; print(json.load(open('${freshness_file}')).get('logged_at', ''))" 2>/dev/null)"
        e2e_log "Index freshness logged: ${freshness_file}"
    fi
}

# sv3_log_model_version: Log semantic model version
#
# Usage:
#   sv3_log_model_version <model_name> <version>
#
sv3_log_model_version() {
    local model_name="$1"
    local version="$2"

    _SV3_SEMANTIC_MODEL_VERSION="${model_name}:${version}"

    local model_file="${SV3_RUN_DIR}/index/model_version.json"
    cat > "${model_file}" <<EOJSON
{
    "logged_at": "$(date -u +%Y-%m-%dT%H:%M:%SZ)",
    "model_name": "$(_e2e_json_escape "${model_name}")",
    "version": "$(_e2e_json_escape "${version}")"
}
EOJSON

    e2e_log "Semantic model: ${model_name} v${version}"
}

# ---------------------------------------------------------------------------
# Summary generation
# ---------------------------------------------------------------------------

# sv3_write_summary: Write human-readable and JSON summaries
#
# Call this at the end of your test script, before e2e_summary.
#
sv3_write_summary() {
    local summary_json="${SV3_RUN_DIR}/summary.json"
    local summary_txt="${SV3_RUN_DIR}/summary.txt"

    local end_time
    end_time="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

    # Calculate mode statistics
    # Pre-compute bash associative array values before passing to Python
    local lexical_count="${_SV3_MODE_SEARCH_COUNT[lexical]:-0}"
    local lexical_ms="${_SV3_MODE_TOTAL_MS[lexical]:-0}"
    local semantic_count="${_SV3_MODE_SEARCH_COUNT[semantic]:-0}"
    local semantic_ms="${_SV3_MODE_TOTAL_MS[semantic]:-0}"
    local hybrid_count="${_SV3_MODE_SEARCH_COUNT[hybrid]:-0}"
    local hybrid_ms="${_SV3_MODE_TOTAL_MS[hybrid]:-0}"
    local auto_count="${_SV3_MODE_SEARCH_COUNT[auto]:-0}"
    local auto_ms="${_SV3_MODE_TOTAL_MS[auto]:-0}"

    local mode_stats_json
    mode_stats_json=$(python3 -c "
import json
modes = {
    'lexical': {'count': ${lexical_count}, 'total_ms': ${lexical_ms}, 'avg_ms': round(${lexical_ms} / ${lexical_count}, 2) if ${lexical_count} > 0 else 0},
    'semantic': {'count': ${semantic_count}, 'total_ms': ${semantic_ms}, 'avg_ms': round(${semantic_ms} / ${semantic_count}, 2) if ${semantic_count} > 0 else 0},
    'hybrid': {'count': ${hybrid_count}, 'total_ms': ${hybrid_ms}, 'avg_ms': round(${hybrid_ms} / ${hybrid_count}, 2) if ${hybrid_count} > 0 else 0},
    'auto': {'count': ${auto_count}, 'total_ms': ${auto_ms}, 'avg_ms': round(${auto_ms} / ${auto_count}, 2) if ${auto_count} > 0 else 0}
}
print(json.dumps(modes, indent=2))
" 2>/dev/null || echo "{}")

    # Write JSON summary
    cat > "${summary_json}" <<EOJSON
{
    "schema_version": 1,
    "harness": "e2e_search_v3.sh",
    "bead": "br-2tnl.7.7",
    "suite": "$(_e2e_json_escape "${E2E_SUITE}")",
    "timestamp": "$(_e2e_json_escape "${E2E_TIMESTAMP}")",
    "run_started_at": "$(_e2e_json_escape "${E2E_RUN_STARTED_AT}")",
    "run_ended_at": "${end_time}",
    "search_stats": {
        "total_searches": ${_SV3_SEARCH_COUNT},
        "ranking_matches": ${_SV3_RANKING_MATCH},
        "ranking_mismatches": ${_SV3_RANKING_MISMATCH}
    },
    "mode_stats": ${mode_stats_json},
    "index_freshness_ts": "$(_e2e_json_escape "${_SV3_INDEX_FRESHNESS_TS}")",
    "semantic_model_version": "$(_e2e_json_escape "${_SV3_SEMANTIC_MODEL_VERSION}")",
    "artifact_dir": "$(_e2e_json_escape "${SV3_RUN_DIR}")"
}
EOJSON

    # Write human-readable summary
    cat > "${summary_txt}" <<EOF
================================================================================
Search V3 E2E Summary (br-2tnl.7.7)
================================================================================

Suite:     ${E2E_SUITE}
Timestamp: ${E2E_TIMESTAMP}
Started:   ${E2E_RUN_STARTED_AT}
Ended:     ${end_time}

Search Statistics
-----------------
Total Searches:      ${_SV3_SEARCH_COUNT}
Ranking Matches:     ${_SV3_RANKING_MATCH}
Ranking Mismatches:  ${_SV3_RANKING_MISMATCH}

Mode Statistics
---------------
EOF

    for mode in lexical semantic hybrid auto; do
        local count="${_SV3_MODE_SEARCH_COUNT[${mode}]:-0}"
        local total="${_SV3_MODE_TOTAL_MS[${mode}]:-0}"
        local avg="0"
        if [ "${count}" -gt 0 ]; then
            avg=$(( total / count ))
        fi
        printf "%-10s: %3d searches, %6d ms total, %4d ms avg\n" \
            "${mode}" "${count}" "${total}" "${avg}" >> "${summary_txt}"
    done

    cat >> "${summary_txt}" <<EOF

Index Freshness:     ${_SV3_INDEX_FRESHNESS_TS:-n/a}
Semantic Model:      ${_SV3_SEMANTIC_MODEL_VERSION:-n/a}

Artifacts: ${SV3_RUN_DIR}
================================================================================
EOF

    e2e_log "Search V3 summary: ${summary_json}"
    cat "${summary_txt}"
}

# ---------------------------------------------------------------------------
# Internal helpers
# ---------------------------------------------------------------------------

_sv3_extract_ranking() {
    local case_dir="$1"
    local response_file="${case_dir}/response.json"
    local ranking_file="${case_dir}/ranking.json"

    python3 -c "
import json
try:
    data = json.load(open('${response_file}'))
    result = data.get('result', {})
    if 'content' in result and result['content']:
        inner = json.loads(result['content'][0]['text'])
    else:
        inner = result

    # Handle both wrapped and unwrapped formats
    if isinstance(inner, dict) and 'result' in inner:
        results = inner['result']
    elif isinstance(inner, list):
        results = inner
    else:
        results = inner.get('results', [])

    ranking = {
        'results': [
            {
                'id': r.get('id') or r.get('message_id'),
                'score': r.get('score', r.get('rank_score', 0)),
                'subject': r.get('subject', '')[:50]
            }
            for r in (results if isinstance(results, list) else [])
        ]
    }
    print(json.dumps(ranking, indent=2))
except Exception as e:
    print(json.dumps({'error': str(e), 'results': []}))
" > "${ranking_file}" 2>/dev/null
}

_sv3_extract_explain() {
    local case_dir="$1"
    local response_file="${case_dir}/response.json"
    local explain_file="${case_dir}/explain.json"

    python3 -c "
import json
try:
    data = json.load(open('${response_file}'))
    result = data.get('result', {})
    if 'content' in result and result['content']:
        inner = json.loads(result['content'][0]['text'])
    else:
        inner = result

    explain = inner.get('explain', inner.get('explanation', {}))
    if explain:
        print(json.dumps(explain, indent=2))
    else:
        print('{}')
except:
    print('{}')
" > "${explain_file}" 2>/dev/null

    # Also save to central explain directory
    local explain_central="${SV3_RUN_DIR}/explain/$(basename "${case_dir}").json"
    cp "${explain_file}" "${explain_central}" 2>/dev/null || true
}

_sv3_trace_search() {
    local case_id="$1"
    local status="$2"
    local mode="$3"
    local elapsed_ms="$4"

    _e2e_trace_event "sv3_search" "case=${case_id} status=${status} mode=${mode} elapsed_ms=${elapsed_ms}"
}

# ---------------------------------------------------------------------------
# Convenience: Combined setup for stdio-based tests
# ---------------------------------------------------------------------------

# sv3_setup_stdio_test: Convenience function for setting up a stdio-based Search V3 test
#
# Usage:
#   sv3_setup_stdio_test <db_var> <project_var>
#
# Sets up a temporary database and project path, storing them in the named variables.
#
sv3_setup_stdio_test() {
    local db_var="$1"
    local project_var="$2"

    local work
    work="$(e2e_mktemp "sv3_stdio")"
    local db="${work}/search_v3.sqlite3"
    local project="/tmp/e2e_sv3_$$"

    eval "${db_var}='${db}'"
    eval "${project_var}='${project}'"

    e2e_log "Search V3 stdio test setup: db=${db} project=${project}"
}

e2e_log "Search V3 harness loaded (br-2tnl.7.7)"
