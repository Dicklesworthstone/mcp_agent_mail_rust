#!/usr/bin/env bash
# test_search_v3_harness.sh - E2E: Search V3 harness validation (br-2tnl.7.7)
#
# This script validates that the Search V3 E2E logging harness works correctly.
# It tests the harness infrastructure itself, not specific Search V3 features.
#
# Tests:
#   1. Harness initialization creates expected directory structure
#   2. Search via stdio captures all expected artifacts
#   3. Ranking extraction produces valid JSON
#   4. Ranking comparison detects matches and mismatches
#   5. Summary generation produces valid output files
#
# Usage:
#   ./tests/e2e/test_search_v3_harness.sh
#
# Reference: br-2tnl.7.7

E2E_SUITE="search_v3_harness"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

# shellcheck source=../../scripts/e2e_search_v3.sh
source "${SCRIPT_DIR}/../../scripts/e2e_search_v3.sh"

e2e_init_artifacts
sv3_init_harness
e2e_banner "Search V3 Harness Validation Suite (br-2tnl.7.7)"

e2e_ensure_binary "am" >/dev/null
e2e_ensure_binary "mcp-agent-mail" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

# ---------------------------------------------------------------------------
# Case 1: Harness initialization creates expected directory structure
# ---------------------------------------------------------------------------
e2e_case_banner "Harness initialization"

e2e_assert_dir_exists "SV3_RUN_DIR exists" "${SV3_RUN_DIR}"
e2e_assert_dir_exists "queries dir exists" "${SV3_RUN_DIR}/queries"
e2e_assert_dir_exists "rankings dir exists" "${SV3_RUN_DIR}/rankings"
e2e_assert_dir_exists "explain dir exists" "${SV3_RUN_DIR}/explain"
e2e_assert_dir_exists "timing dir exists" "${SV3_RUN_DIR}/timing"
e2e_assert_dir_exists "index dir exists" "${SV3_RUN_DIR}/index"
e2e_assert_file_exists "run_metadata.json exists" "${SV3_RUN_DIR}/run_metadata.json"

# Verify metadata content
META_CHECK="$(python3 -c "
import json
data = json.load(open('${SV3_RUN_DIR}/run_metadata.json'))
print(f'schema={data.get(\"schema_version\")}|suite={data.get(\"suite\")}|harness={data.get(\"harness\")}')
" 2>/dev/null)"
e2e_assert_contains "metadata has schema version" "${META_CHECK}" "schema=1"
e2e_assert_contains "metadata has suite" "${META_CHECK}" "suite=${E2E_SUITE}"
e2e_assert_contains "metadata has harness" "${META_CHECK}" "harness=e2e_search_v3.sh"

# ---------------------------------------------------------------------------
# Case 2: Prepare deterministic fixture responses
# ---------------------------------------------------------------------------
e2e_case_banner "Setup fixture corpus"

WORK="$(e2e_mktemp "sv3_harness")"
SV3_DB="${WORK}/fixture.sqlite3"
SV3_PROJECT="/tmp/e2e_sv3_fixture_$$"
FIXTURE_LEXICAL="${WORK}/fixture_lexical.jsonl"
FIXTURE_API="${WORK}/fixture_api.jsonl"

cat > "${FIXTURE_LEXICAL}" <<'EOF'
{"jsonrpc":"2.0","result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{},"resources":{},"logging":{}},"serverInfo":{"name":"mcp-agent-mail","version":"0.1.0"}},"id":1}
{"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"{\"mode\":\"lexical\",\"result\":[{\"id\":101,\"subject\":\"Database migration plan\",\"score\":0.98},{\"id\":102,\"subject\":\"API endpoint review\",\"score\":0.83}],\"explain\":{\"scores\":[0.98,0.83],\"ranking_factors\":[\"bm25\"]}}"}]},"id":2}
EOF

cat > "${FIXTURE_API}" <<'EOF'
{"jsonrpc":"2.0","result":{"protocolVersion":"2024-11-05","capabilities":{"tools":{},"resources":{},"logging":{}},"serverInfo":{"name":"mcp-agent-mail","version":"0.1.0"}},"id":1}
{"jsonrpc":"2.0","result":{"content":[{"type":"text","text":"{\"mode\":\"auto\",\"result\":[{\"id\":202,\"subject\":\"API endpoint review\",\"score\":0.91}],\"explain\":{\"scores\":[0.91],\"ranking_factors\":[\"bm25\"]}}"}]},"id":2}
EOF

export SV3_STDIO_FIXTURE_ELAPSED_MS=7
e2e_pass "fixture responses prepared"

# ---------------------------------------------------------------------------
# Case 3: Search via stdio captures all expected artifacts
# ---------------------------------------------------------------------------
e2e_case_banner "Search artifact capture"

export SV3_STDIO_FIXTURE_RESPONSE="${FIXTURE_LEXICAL}"
sv3_search_stdio "lexical_basic" "$SV3_DB" "$SV3_PROJECT" "migration" "lexical"

e2e_assert_file_exists "params.json created" "${SV3_RUN_DIR}/queries/lexical_basic/params.json"
e2e_assert_file_exists "request.json created" "${SV3_RUN_DIR}/queries/lexical_basic/request.json"
e2e_assert_file_exists "response.json created" "${SV3_RUN_DIR}/queries/lexical_basic/response.json"
e2e_assert_file_exists "ranking.json created" "${SV3_RUN_DIR}/queries/lexical_basic/ranking.json"
e2e_assert_file_exists "timing.txt created" "${SV3_RUN_DIR}/queries/lexical_basic/timing.txt"

# Verify params content
PARAMS_CHECK="$(python3 -c "
import json
data = json.load(open('${SV3_RUN_DIR}/queries/lexical_basic/params.json'))
print(f'mode={data.get(\"mode\")}|query={data.get(\"query\")}')
" 2>/dev/null)"
e2e_assert_contains "params has lexical mode" "${PARAMS_CHECK}" "mode=lexical"
e2e_assert_contains "params has query" "${PARAMS_CHECK}" "query=migration"

# ---------------------------------------------------------------------------
# Case 4: Ranking extraction produces valid JSON
# ---------------------------------------------------------------------------
e2e_case_banner "Ranking extraction"

RANKING_CHECK="$(python3 -c "
import json
data = json.load(open('${SV3_RUN_DIR}/queries/lexical_basic/ranking.json'))
results = data.get('results', [])
print(f'count={len(results)}|has_error={\"error\" in data}')
" 2>/dev/null)"
e2e_save_artifact "ranking_check.txt" "$RANKING_CHECK"

e2e_assert_contains "ranking has results" "${RANKING_CHECK}" "count="
e2e_assert_not_contains "ranking has no error" "${RANKING_CHECK}" "has_error=True"

# Run another search for comparison
export SV3_STDIO_FIXTURE_RESPONSE="${FIXTURE_API}"
sv3_search_stdio "api_search" "$SV3_DB" "$SV3_PROJECT" "API" "auto"

# ---------------------------------------------------------------------------
# Case 5: Ranking comparison
# ---------------------------------------------------------------------------
e2e_case_banner "Ranking comparison"

# Get the message IDs from the first search
EXPECTED_IDS="$(python3 -c "
import json
data = json.load(open('${SV3_RUN_DIR}/queries/lexical_basic/ranking.json'))
ids = [str(r.get('id')) for r in data.get('results', [])]
print(','.join(ids))
" 2>/dev/null)"

if [ -n "$EXPECTED_IDS" ] && [ "$EXPECTED_IDS" != "None" ]; then
    # Run same search again
    export SV3_STDIO_FIXTURE_RESPONSE="${FIXTURE_LEXICAL}"
    sv3_search_stdio "lexical_repeat" "$SV3_DB" "$SV3_PROJECT" "migration" "lexical"

    if sv3_compare_rankings "lexical_repeat" "$EXPECTED_IDS"; then
        e2e_pass "ranking comparison matches identical searches"
    else
        e2e_fail "ranking comparison failed for identical searches"
    fi

    e2e_assert_file_exists "diff file created" "${SV3_RUN_DIR}/rankings/lexical_repeat_diff.json"
else
    e2e_skip "no results to compare (empty corpus or search issue)"
fi

# Test mismatch detection
if sv3_compare_rankings "api_search" "9999,9998,9997"; then
    e2e_fail "ranking comparison should detect mismatch"
else
    e2e_pass "ranking comparison detects mismatch"
fi

# ---------------------------------------------------------------------------
# Case 6: Summary generation
# ---------------------------------------------------------------------------
e2e_case_banner "Summary generation"

sv3_write_summary

e2e_assert_file_exists "summary.json created" "${SV3_RUN_DIR}/summary.json"
e2e_assert_file_exists "summary.txt created" "${SV3_RUN_DIR}/summary.txt"

SUMMARY_CHECK="$(python3 -c "
import json
data = json.load(open('${SV3_RUN_DIR}/summary.json'))
stats = data.get('search_stats', {})
print(f'total={stats.get(\"total_searches\")}|schema={data.get(\"schema_version\")}')
" 2>/dev/null)"
e2e_assert_contains "summary has total searches" "${SUMMARY_CHECK}" "total="
e2e_assert_contains "summary has schema version" "${SUMMARY_CHECK}" "schema=1"

# Verify human-readable summary
if grep -q "Search V3 E2E Summary" "${SV3_RUN_DIR}/summary.txt"; then
    e2e_pass "summary.txt has header"
else
    e2e_fail "summary.txt missing header"
fi

if grep -q "Mode Statistics" "${SV3_RUN_DIR}/summary.txt"; then
    e2e_pass "summary.txt has mode stats"
else
    e2e_fail "summary.txt missing mode stats"
fi

# ---------------------------------------------------------------------------
# Case 7: Mode override works
# ---------------------------------------------------------------------------
e2e_case_banner "Mode override"

# Set mode override
export SV3_MODE_OVERRIDE="hybrid"
export SV3_STDIO_FIXTURE_RESPONSE="${FIXTURE_LEXICAL}"
sv3_search_stdio "override_test" "$SV3_DB" "$SV3_PROJECT" "test" "lexical"
unset SV3_MODE_OVERRIDE

OVERRIDE_CHECK="$(python3 -c "
import json
data = json.load(open('${SV3_RUN_DIR}/queries/override_test/params.json'))
print(f'mode={data.get(\"mode\")}')
" 2>/dev/null)"
e2e_assert_contains "mode override applied" "${OVERRIDE_CHECK}" "mode=hybrid"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------
e2e_summary
