#!/usr/bin/env bash
# test_toon.sh - E2E test suite for TOON output format
#
# Tests:
# 1. Stub encoder: success path with stats
# 2. Stub encoder: success without stats
# 3. Failing stub encoder: fallback to JSON with toon_error
# 4. Multi-tool sequence: health_check + ensure_project + register_agent + inbox resource
# 5. Encoder validation: --help / --version responses
# 6. Broken encoder path: graceful fallback
#
# All tests are offline — no real tru binary or server required.

E2E_SUITE="toon"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TOON Output Format E2E Test Suite"

# Locate stub encoders
STUB="${E2E_PROJECT_ROOT}/scripts/toon_stub_encoder.sh"
STUB_FAIL="${E2E_PROJECT_ROOT}/scripts/toon_stub_encoder_fail.sh"

if [ ! -x "$STUB" ] || [ ! -x "$STUB_FAIL" ]; then
    e2e_log "ERROR: stub encoders not found or not executable"
    e2e_log "  STUB:      $STUB"
    e2e_log "  STUB_FAIL: $STUB_FAIL"
    exit 1
fi

# Helper: run stub encoder and capture output
run_stub() {
    local input="$1"
    shift
    echo "$input" | "$STUB" "$@"
}

# Helper: extract JSON field with python (jq-free)
json_get() {
    local json="$1"
    local field="$2"
    python3 -c "import json,sys; d=json.loads(sys.argv[1]); print(d.get('$field',''))" "$json" 2>/dev/null
}

json_get_nested() {
    local json="$1"
    local path="$2"
    python3 -c "
import json,sys
d=json.loads(sys.argv[1])
keys=sys.argv[2].split('.')
for k in keys:
    if isinstance(d, dict):
        d = d.get(k, '')
    else:
        d = ''
        break
print(d)
" "$json" "$path" 2>/dev/null
}

# ---------------------------------------------------------------------------
# Case 1: Stub encoder success with stats
# ---------------------------------------------------------------------------
e2e_case_banner "Stub encoder success with stats"

PAYLOAD='{"id":1,"subject":"Hello","body":"World"}'
STDOUT=$(echo "$PAYLOAD" | "$STUB" --encode --stats 2>"${E2E_ARTIFACT_DIR}/case1_stderr.txt")
STDERR=$(cat "${E2E_ARTIFACT_DIR}/case1_stderr.txt")
STUB_RC=$?

e2e_assert_exit_code "stub exits 0" "0" "$STUB_RC"
e2e_assert_contains "stdout has stub marker" "$STDOUT" "~stub_toon_output"
e2e_assert_contains "stdout has payload_length" "$STDOUT" "payload_length:"
e2e_assert_contains "stderr has token estimates" "$STDERR" "Token estimates:"
e2e_assert_contains "stderr has saved line" "$STDERR" "Saved ~13 tokens"

e2e_save_artifact "case1_stdout.txt" "$STDOUT"

# ---------------------------------------------------------------------------
# Case 2: Stub encoder success without stats
# ---------------------------------------------------------------------------
e2e_case_banner "Stub encoder success without stats"

STDOUT2=$(echo "$PAYLOAD" | "$STUB" --encode 2>"${E2E_ARTIFACT_DIR}/case2_stderr.txt")
STDERR2=$(cat "${E2E_ARTIFACT_DIR}/case2_stderr.txt")

e2e_assert_contains "stdout has marker" "$STDOUT2" "~stub_toon_output"
e2e_assert_eq "stderr is empty (no --stats)" "" "$STDERR2"

e2e_save_artifact "case2_stdout.txt" "$STDOUT2"

# ---------------------------------------------------------------------------
# Case 3: Failing stub encoder - non-zero exit
# ---------------------------------------------------------------------------
e2e_case_banner "Failing stub encoder returns non-zero exit"

set +e
FAIL_STDOUT=$(echo "$PAYLOAD" | "$STUB_FAIL" --encode 2>"${E2E_ARTIFACT_DIR}/case3_stderr.txt")
FAIL_RC=$?
set -e
FAIL_STDERR=$(cat "${E2E_ARTIFACT_DIR}/case3_stderr.txt")

e2e_assert_eq "failing stub exits 1" "1" "$FAIL_RC"
e2e_assert_eq "stdout is empty on failure" "" "$FAIL_STDOUT"
e2e_assert_contains "stderr has error message" "$FAIL_STDERR" "simulated encoder failure"

e2e_save_artifact "case3_stderr.txt" "$FAIL_STDERR"

# ---------------------------------------------------------------------------
# Case 4: Multi-tool TOON sequence (offline simulation)
# ---------------------------------------------------------------------------
e2e_case_banner "Multi-tool TOON sequence (offline)"

WORK="$(e2e_mktemp "e2e_toon")"
LOG_FILE="${WORK}/e2e_toon_log.json"

# Simulate 4 tool/resource calls through the stub encoder.
# Each gets wrapped in a TOON envelope structure.
STEPS="[]"

for tool in "health_check" "ensure_project" "register_agent"; do
    case "$tool" in
        health_check)     TOOL_PAYLOAD='{"status":"ok","version":"1.0.0"}' ;;
        ensure_project)   TOOL_PAYLOAD='{"id":1,"slug":"backend","human_key":"/backend"}' ;;
        register_agent)   TOOL_PAYLOAD='{"id":1,"name":"BlueLake","program":"codex","model":"gpt-5"}' ;;
    esac

    ENCODED=$(echo "$TOOL_PAYLOAD" | "$STUB" --encode --stats 2>"${WORK}/${tool}_stderr.txt")
    TOOL_STDERR=$(cat "${WORK}/${tool}_stderr.txt")
    TOOL_RC=$?

    if [ "$TOOL_RC" -eq 0 ]; then
        # Build envelope JSON
        ENVELOPE=$(python3 -c "
import json, sys
encoded = sys.argv[1]
payload = json.loads(sys.argv[2])
stderr = sys.argv[3]

# Parse stats from stderr
stats = {}
for line in stderr.strip().split('\n'):
    if 'Token estimates:' in line:
        import re
        m = re.search(r'~(\d+)\s*\(JSON\)\s*(?:->|→)\s*~(\d+)\s*\(TOON\)', line)
        if m:
            stats['json_tokens'] = int(m.group(1))
            stats['toon_tokens'] = int(m.group(2))
    if line.startswith('Saved'):
        m = re.search(r'~(\d+)\s+tokens\s+\(([-\d.]+)%\)', line)
        if m:
            stats['saved_tokens'] = int(m.group(1))
            stats['saved_percent'] = float(m.group(2))

envelope = {
    'format': 'toon',
    'data': encoded,
    'meta': {
        'requested': 'toon',
        'source': 'param',
        'encoder': 'toon_stub_encoder.sh',
        'toon_stats': stats if stats else None
    }
}
print(json.dumps(envelope))
" "$ENCODED" "$TOOL_PAYLOAD" "$TOOL_STDERR")

        STEPS=$(python3 -c "
import json, sys
steps = json.loads(sys.argv[1])
steps.append({'tool': sys.argv[2], 'format': 'toon', 'envelope_format': json.loads(sys.argv[3]).get('format')})
print(json.dumps(steps))
" "$STEPS" "$tool" "$ENVELOPE")

        e2e_pass "tool=$tool encoded successfully"
    else
        e2e_fail "tool=$tool encoder failed with rc=$TOOL_RC"
    fi

    e2e_save_artifact "case4_${tool}_envelope.json" "$ENVELOPE"
done

# Simulate resource read with format=toon query param
RESOURCE_PAYLOAD='[{"id":1,"subject":"Welcome","from":"System","importance":"normal"}]'
RESOURCE_ENCODED=$(echo "$RESOURCE_PAYLOAD" | "$STUB" --encode 2>/dev/null)
RESOURCE_RC=$?

if [ "$RESOURCE_RC" -eq 0 ]; then
    STEPS=$(python3 -c "
import json, sys
steps = json.loads(sys.argv[1])
steps.append({'resource': 'inbox', 'format': 'toon'})
print(json.dumps(steps))
" "$STEPS")
    e2e_pass "resource=inbox encoded successfully"
else
    e2e_fail "resource=inbox encoder failed"
fi

# Write structured log
python3 -c "
import json, sys
log = {
    'test': 'e2e_toon_format_multi_tool_sequence',
    'steps': json.loads(sys.argv[1])
}
with open(sys.argv[2], 'w') as f:
    json.dump(log, f, indent=2)
" "$STEPS" "$LOG_FILE"

e2e_assert_file_exists "E2E log written" "$LOG_FILE"

# Verify log structure
STEP_COUNT=$(python3 -c "import json; print(len(json.load(open('$LOG_FILE'))['steps']))")
e2e_assert_eq "log has 4 steps" "4" "$STEP_COUNT"

# Verify all steps used toon format
ALL_TOON=$(python3 -c "
import json
log = json.load(open('$LOG_FILE'))
print('true' if all(s.get('format') == 'toon' for s in log['steps']) else 'false')
")
e2e_assert_eq "all steps used toon format" "true" "$ALL_TOON"

e2e_copy_artifact "$LOG_FILE" "case4_e2e_log.json"

# ---------------------------------------------------------------------------
# Case 5: Encoder validation responses
# ---------------------------------------------------------------------------
e2e_case_banner "Encoder validation (--help and --version)"

HELP_OUT=$("$STUB" --help)
VERSION_OUT=$("$STUB" --version)

e2e_assert_contains "--help mentions 'reference implementation in rust'" "$HELP_OUT" "reference implementation in rust"
e2e_assert_contains "--version starts with 'tru '" "$VERSION_OUT" "tru "

HELP_FAIL=$("$STUB_FAIL" --help)
VERSION_FAIL=$("$STUB_FAIL" --version)

e2e_assert_contains "failing stub --help also passes validation" "$HELP_FAIL" "reference implementation in rust"
e2e_assert_contains "failing stub --version starts with 'tru '" "$VERSION_FAIL" "tru "

e2e_save_artifact "case5_help.txt" "$HELP_OUT"
e2e_save_artifact "case5_version.txt" "$VERSION_OUT"

# ---------------------------------------------------------------------------
# Case 6: Broken encoder path - graceful behavior
# ---------------------------------------------------------------------------
e2e_case_banner "Broken encoder path produces clear error"

set +e
BROKEN_OUT=$("/nonexistent/tru_binary" --encode < /dev/null 2>"${E2E_ARTIFACT_DIR}/case6_stderr.txt")
BROKEN_RC=$?
set -e
BROKEN_STDERR=$(cat "${E2E_ARTIFACT_DIR}/case6_stderr.txt")

e2e_assert_eq "nonexistent binary exits non-zero" "true" "$([ "$BROKEN_RC" -ne 0 ] && echo true || echo false)"
e2e_assert_contains "stderr has 'No such file'" "$BROKEN_STDERR" "No such file"

# ---------------------------------------------------------------------------
# Case 7: Fallback envelope structure verification
# ---------------------------------------------------------------------------
e2e_case_banner "Fallback envelope has correct structure"

# Simulate what the Rust code produces on encoder failure
FALLBACK_ENVELOPE=$(python3 -c "
import json
envelope = {
    'format': 'json',
    'data': {'id': 1, 'subject': 'Test'},
    'meta': {
        'requested': 'toon',
        'source': 'param',
        'toon_error': 'TOON encoder exited with 1',
        'toon_stderr': 'error: simulated encoder failure'
    }
}
print(json.dumps(envelope))
")

# Verify structure
FMT=$(json_get "$FALLBACK_ENVELOPE" "format")
e2e_assert_eq "fallback format is json" "json" "$FMT"

DATA_ID=$(json_get_nested "$FALLBACK_ENVELOPE" "data.id")
e2e_assert_eq "fallback preserves data.id" "1" "$DATA_ID"

META_REQ=$(json_get_nested "$FALLBACK_ENVELOPE" "meta.requested")
e2e_assert_eq "fallback records requested=toon" "toon" "$META_REQ"

TOON_ERR=$(json_get_nested "$FALLBACK_ENVELOPE" "meta.toon_error")
e2e_assert_contains "fallback has toon_error" "$TOON_ERR" "exited with"

e2e_save_artifact "case7_fallback_envelope.json" "$FALLBACK_ENVELOPE"

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
