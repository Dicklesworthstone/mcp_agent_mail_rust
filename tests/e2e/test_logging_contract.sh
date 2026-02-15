#!/usr/bin/env bash
# test_logging_contract.sh - Validate structured logging contract (br-1xt0m.1.13.13)
#
# Verifies:
# - Trace events use schema_version 2
# - assert_pass/assert_fail/assert_skip events include assertion_id and elapsed_ms
# - step_start/step_end events include step name and elapsed_ms
# - case_start events reset assertion counter
# - Bundle manifest references trace-events.v2 schema
# - Deterministic mode produces stable assertion IDs

E2E_SUITE="logging_contract"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Test Logging Contract E2E Suite (br-1xt0m.1.13.13)"

if ! command -v python3 >/dev/null 2>&1; then
    e2e_log "python3 not found; skipping suite"
    e2e_skip "python3 required for trace validation"
    e2e_summary
    exit 0
fi

# ---------------------------------------------------------------------------
# Case 1: Assert events carry assertion_id and elapsed_ms
# ---------------------------------------------------------------------------
e2e_case_banner "Assert events carry assertion_id and elapsed_ms"

# Create a minimal sub-suite artifact tree so we can inspect the trace
SUB_DIR="$(e2e_mktemp "logging_contract_sub")"
mkdir -p "${SUB_DIR}/trace"
_SUB_TRACE="${SUB_DIR}/trace/events.jsonl"

# Temporarily redirect trace output to the sub trace file
_SAVED_TRACE_FILE="$_E2E_TRACE_FILE"
_E2E_TRACE_FILE="$_SUB_TRACE"

# Simulate a mini test run
_E2E_CURRENT_CASE=""
_E2E_ASSERT_SEQ=0
_E2E_CASE_START_MS=0

# Emit events: case_start -> pass -> fail -> skip
_E2E_CURRENT_CASE="test_case_alpha"
_E2E_ASSERT_SEQ=0
_E2E_CASE_START_MS="$(_e2e_now_ms)"
_e2e_trace_event "case_start" "" "test_case_alpha"

# Emit a pass with assertion_id
(( _E2E_ASSERT_SEQ++ )) || true
local_aid="test_case_alpha.a1"
local_now="$(_e2e_now_ms)"
local_elapsed=$(( local_now - _E2E_CASE_START_MS ))
_e2e_trace_event "assert_pass" "first assertion" "" "$local_aid" "" "$local_elapsed"

# Emit a fail with assertion_id
(( _E2E_ASSERT_SEQ++ )) || true
local_aid2="test_case_alpha.a2"
local_now2="$(_e2e_now_ms)"
local_elapsed2=$(( local_now2 - _E2E_CASE_START_MS ))
_e2e_trace_event "assert_fail" "second assertion" "" "$local_aid2" "" "$local_elapsed2"

# Emit step_start / step_end
_e2e_trace_event "step_start" "" "" "" "setup_env"
_e2e_trace_event "step_end" "" "" "" "setup_env" "42"

# Restore trace file
_E2E_TRACE_FILE="$_SAVED_TRACE_FILE"

# Validate the sub-trace via python
python3 - "$_SUB_TRACE" <<'PY'
import json
import sys

trace_path = sys.argv[1]
events = []
with open(trace_path, "r") as f:
    for line in f:
        line = line.strip()
        if line:
            events.append(json.loads(line))

errors = []

# All events should be schema_version 2
for i, ev in enumerate(events):
    if ev.get("schema_version") != 2:
        errors.append(f"event {i}: schema_version={ev.get('schema_version')}, expected 2")

# Find assert_pass and assert_fail events
pass_events = [e for e in events if e["kind"] == "assert_pass"]
fail_events = [e for e in events if e["kind"] == "assert_fail"]
step_start = [e for e in events if e["kind"] == "step_start"]
step_end = [e for e in events if e["kind"] == "step_end"]

if len(pass_events) != 1:
    errors.append(f"expected 1 assert_pass, got {len(pass_events)}")
if len(fail_events) != 1:
    errors.append(f"expected 1 assert_fail, got {len(fail_events)}")

# Check assertion_id present
for ev in pass_events + fail_events:
    aid = ev.get("assertion_id")
    if not aid or not isinstance(aid, str):
        errors.append(f"{ev['kind']}: missing or invalid assertion_id")

# Check assertion_id format: case.aN
if pass_events:
    if pass_events[0].get("assertion_id") != "test_case_alpha.a1":
        errors.append(f"pass assertion_id={pass_events[0].get('assertion_id')}, expected test_case_alpha.a1")
if fail_events:
    if fail_events[0].get("assertion_id") != "test_case_alpha.a2":
        errors.append(f"fail assertion_id={fail_events[0].get('assertion_id')}, expected test_case_alpha.a2")

# Check elapsed_ms present and numeric
for ev in pass_events + fail_events:
    ems = ev.get("elapsed_ms")
    if not isinstance(ems, (int, float)):
        errors.append(f"{ev['kind']}: elapsed_ms missing or not numeric, got {type(ems).__name__}")
    elif ems < 0:
        errors.append(f"{ev['kind']}: elapsed_ms negative: {ems}")

# Check step events
if len(step_start) != 1:
    errors.append(f"expected 1 step_start, got {len(step_start)}")
if len(step_end) != 1:
    errors.append(f"expected 1 step_end, got {len(step_end)}")

for ev in step_start + step_end:
    step = ev.get("step")
    if step != "setup_env":
        errors.append(f"{ev['kind']}: step={step}, expected setup_env")

if step_end:
    ems = step_end[0].get("elapsed_ms")
    if ems != 42:
        errors.append(f"step_end elapsed_ms={ems}, expected 42")

if errors:
    print("ERRORS:", file=sys.stderr)
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    sys.exit(1)
PY

if [ $? -eq 0 ]; then
    e2e_pass "assert events carry assertion_id, elapsed_ms, and step fields"
else
    e2e_fail "trace event v2 contract violation"
fi

# ---------------------------------------------------------------------------
# Case 2: e2e_pass/e2e_fail/e2e_skip auto-generate assertion IDs
# ---------------------------------------------------------------------------
e2e_case_banner "Auto-generated assertion IDs from helpers"

SUB_DIR2="$(e2e_mktemp "logging_contract_auto")"
mkdir -p "${SUB_DIR2}/trace"
_SUB_TRACE2="${SUB_DIR2}/trace/events.jsonl"

_SAVED_TRACE_FILE="$_E2E_TRACE_FILE"
_SAVED_PASS=$_E2E_PASS
_SAVED_FAIL=$_E2E_FAIL
_SAVED_SKIP=$_E2E_SKIP
_SAVED_TOTAL=$_E2E_TOTAL
_SAVED_CASE="$_E2E_CURRENT_CASE"
_SAVED_ASEQ=$_E2E_ASSERT_SEQ
_SAVED_CASE_MS=$_E2E_CASE_START_MS

# Redirect to sub-trace
_E2E_TRACE_FILE="$_SUB_TRACE2"
_E2E_PASS=0
_E2E_FAIL=0
_E2E_SKIP=0
_E2E_TOTAL=0

# Simulate using the public API
e2e_case_banner "auto_id_case"
e2e_pass "auto pass 1" >/dev/null
e2e_pass "auto pass 2" >/dev/null

# Restore
_E2E_TRACE_FILE="$_SAVED_TRACE_FILE"
_E2E_PASS=$_SAVED_PASS
_E2E_FAIL=$_SAVED_FAIL
_E2E_SKIP=$_SAVED_SKIP
_E2E_TOTAL=$_SAVED_TOTAL
_E2E_CURRENT_CASE="$_SAVED_CASE"
_E2E_ASSERT_SEQ=$_SAVED_ASEQ
_E2E_CASE_START_MS=$_SAVED_CASE_MS

python3 - "$_SUB_TRACE2" <<'PY'
import json
import sys

trace_path = sys.argv[1]
events = []
with open(trace_path, "r") as f:
    for line in f:
        line = line.strip()
        if line:
            events.append(json.loads(line))

errors = []

pass_events = [e for e in events if e["kind"] == "assert_pass"]
if len(pass_events) != 2:
    errors.append(f"expected 2 assert_pass, got {len(pass_events)}")

# Assertion IDs should auto-increment within case
expected_ids = ["auto_id_case.a1", "auto_id_case.a2"]
for i, ev in enumerate(pass_events):
    aid = ev.get("assertion_id", "")
    if i < len(expected_ids) and aid != expected_ids[i]:
        errors.append(f"pass[{i}] assertion_id={aid}, expected {expected_ids[i]}")
    if not isinstance(ev.get("elapsed_ms"), (int, float)):
        errors.append(f"pass[{i}]: elapsed_ms missing or not numeric")

# case_start should NOT have assertion_id
case_starts = [e for e in events if e["kind"] == "case_start"]
for ev in case_starts:
    if "assertion_id" in ev and ev["assertion_id"]:
        errors.append(f"case_start should not have assertion_id, got {ev['assertion_id']}")

if errors:
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    sys.exit(1)
PY

if [ $? -eq 0 ]; then
    e2e_pass "auto-generated assertion IDs increment correctly"
else
    e2e_fail "auto-generated assertion IDs broken"
fi

# ---------------------------------------------------------------------------
# Case 3: e2e_step_start/e2e_step_end produce valid trace events
# ---------------------------------------------------------------------------
e2e_case_banner "Step helpers produce valid trace events"

SUB_DIR3="$(e2e_mktemp "logging_contract_step")"
mkdir -p "${SUB_DIR3}/trace"
_SUB_TRACE3="${SUB_DIR3}/trace/events.jsonl"

_SAVED_TRACE_FILE="$_E2E_TRACE_FILE"
_SAVED_STEP="$_E2E_CURRENT_STEP"
_SAVED_STEP_MS=$_E2E_STEP_START_MS

_E2E_TRACE_FILE="$_SUB_TRACE3"
_E2E_CURRENT_CASE="step_test_case"
_E2E_CURRENT_STEP=""
_E2E_STEP_START_MS=0

e2e_step_start "provision_db"
sleep 0.01  # Ensure some elapsed time
e2e_step_end "provision_db"

e2e_step_start "run_query"
e2e_step_end

_E2E_TRACE_FILE="$_SAVED_TRACE_FILE"
_E2E_CURRENT_STEP="$_SAVED_STEP"
_E2E_STEP_START_MS=$_SAVED_STEP_MS

python3 - "$_SUB_TRACE3" <<'PY'
import json
import sys

trace_path = sys.argv[1]
events = []
with open(trace_path, "r") as f:
    for line in f:
        line = line.strip()
        if line:
            events.append(json.loads(line))

errors = []

starts = [e for e in events if e["kind"] == "step_start"]
ends = [e for e in events if e["kind"] == "step_end"]

if len(starts) != 2:
    errors.append(f"expected 2 step_start, got {len(starts)}")
if len(ends) != 2:
    errors.append(f"expected 2 step_end, got {len(ends)}")

# First step pair: provision_db
if starts and starts[0].get("step") != "provision_db":
    errors.append(f"first step_start step={starts[0].get('step')}, expected provision_db")
if ends and ends[0].get("step") != "provision_db":
    errors.append(f"first step_end step={ends[0].get('step')}, expected provision_db")

# step_end should have elapsed_ms >= 0
for ev in ends:
    ems = ev.get("elapsed_ms")
    if not isinstance(ems, (int, float)):
        errors.append(f"step_end for {ev.get('step')}: elapsed_ms missing or not numeric")
    elif ems < 0:
        errors.append(f"step_end for {ev.get('step')}: elapsed_ms negative: {ems}")

# Second step should use implicit name from e2e_step_end (no arg)
if len(starts) > 1 and starts[1].get("step") != "run_query":
    errors.append(f"second step_start step={starts[1].get('step')}, expected run_query")
if len(ends) > 1 and ends[1].get("step") != "run_query":
    errors.append(f"second step_end step={ends[1].get('step')}, expected run_query")

if errors:
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    sys.exit(1)
PY

if [ $? -eq 0 ]; then
    e2e_pass "step_start/step_end produce valid trace events with timing"
else
    e2e_fail "step trace event contract violation"
fi

# ---------------------------------------------------------------------------
# Case 4: Bundle manifest references trace-events.v2 schema
# ---------------------------------------------------------------------------
e2e_case_banner "Bundle manifest references trace-events.v2"

# The real artifact dir is being populated as we go; let's check the
# bundle manifest template references the correct schema by running
# a mini artifact write and inspecting the JSON.
BUNDLE_DIR="$(e2e_mktemp "logging_contract_bundle")"
mkdir -p "${BUNDLE_DIR}/diagnostics" "${BUNDLE_DIR}/trace" "${BUNDLE_DIR}/transcript" \
         "${BUNDLE_DIR}/logs" "${BUNDLE_DIR}/screenshots"

E2E_RUN_ENDED_AT="$(_e2e_now_rfc3339)"
E2E_RUN_END_EPOCH_S="$(date +%s)"

# Write minimal valid trace for the bundle
cat > "${BUNDLE_DIR}/trace/events.jsonl" <<EOF
{"schema_version":2,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_STARTED_AT}","kind":"suite_start","case":"","message":"","counters":{"total":0,"pass":0,"fail":0,"skip":0}}
{"schema_version":2,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_ENDED_AT}","kind":"suite_end","case":"","message":"","counters":{"total":0,"pass":0,"fail":0,"skip":0}}
EOF

e2e_write_summary_json "${BUNDLE_DIR}"
e2e_write_meta_json "${BUNDLE_DIR}"
e2e_write_metrics_json "${BUNDLE_DIR}"
e2e_write_diagnostics_files "${BUNDLE_DIR}"
e2e_write_transcript_summary "${BUNDLE_DIR}"
e2e_write_repro_files "${BUNDLE_DIR}"
e2e_write_forensic_indexes "${BUNDLE_DIR}"
e2e_write_bundle_manifest "${BUNDLE_DIR}"

# Check that the bundle references trace-events.v2
python3 - "${BUNDLE_DIR}/bundle.json" <<'PY'
import json
import sys

with open(sys.argv[1], "r") as f:
    bundle = json.load(f)

errors = []

# Check trace schema reference
trace = bundle.get("artifacts", {}).get("trace", {}).get("events", {})
schema = trace.get("schema", "")
if schema != "trace-events.v2":
    errors.append(f"artifacts.trace.events.schema={schema}, expected trace-events.v2")

# Check that trace file entry in files array also has v2 schema
for fe in bundle.get("files", []):
    if fe.get("path") == "trace/events.jsonl":
        fs = fe.get("schema", "")
        if fs != "trace-events.v2":
            errors.append(f"files[trace/events.jsonl].schema={fs}, expected trace-events.v2")

if errors:
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    sys.exit(1)
PY

if [ $? -eq 0 ]; then
    e2e_pass "bundle.json references trace-events.v2 schema"
else
    e2e_fail "bundle.json has wrong trace schema version"
fi

# Also validate the bundle passes the full validator
if e2e_validate_bundle_manifest "${BUNDLE_DIR}"; then
    e2e_pass "bundle with v2 traces passes full validation"
else
    e2e_fail "bundle with v2 traces fails validation"
fi

# ---------------------------------------------------------------------------
# Case 5: Deterministic mode produces stable assertion IDs
# ---------------------------------------------------------------------------
e2e_case_banner "Deterministic mode produces stable assertion IDs"

SUB_DIR5="$(e2e_mktemp "logging_contract_det")"
mkdir -p "${SUB_DIR5}/trace"
_SUB_TRACE5="${SUB_DIR5}/trace/events.jsonl"

_SAVED_TRACE_FILE="$_E2E_TRACE_FILE"
_SAVED_PASS=$_E2E_PASS
_SAVED_FAIL=$_E2E_FAIL
_SAVED_SKIP=$_E2E_SKIP
_SAVED_TOTAL=$_E2E_TOTAL
_SAVED_CASE="$_E2E_CURRENT_CASE"
_SAVED_ASEQ=$_E2E_ASSERT_SEQ
_SAVED_CASE_MS=$_E2E_CASE_START_MS

_E2E_TRACE_FILE="$_SUB_TRACE5"
_E2E_PASS=0
_E2E_FAIL=0
_E2E_SKIP=0
_E2E_TOTAL=0

e2e_case_banner "det_case"
e2e_pass "det assertion 1" >/dev/null
e2e_pass "det assertion 2" >/dev/null
e2e_skip "det skipped" >/dev/null

# Run again to a second trace file and compare assertion IDs
SUB_DIR5B="$(e2e_mktemp "logging_contract_det_b")"
mkdir -p "${SUB_DIR5B}/trace"
_SUB_TRACE5B="${SUB_DIR5B}/trace/events.jsonl"
_E2E_TRACE_FILE="$_SUB_TRACE5B"
_E2E_PASS=0
_E2E_FAIL=0
_E2E_SKIP=0
_E2E_TOTAL=0

e2e_case_banner "det_case"
e2e_pass "det assertion 1" >/dev/null
e2e_pass "det assertion 2" >/dev/null
e2e_skip "det skipped" >/dev/null

_E2E_TRACE_FILE="$_SAVED_TRACE_FILE"
_E2E_PASS=$_SAVED_PASS
_E2E_FAIL=$_SAVED_FAIL
_E2E_SKIP=$_SAVED_SKIP
_E2E_TOTAL=$_SAVED_TOTAL
_E2E_CURRENT_CASE="$_SAVED_CASE"
_E2E_ASSERT_SEQ=$_SAVED_ASEQ
_E2E_CASE_START_MS=$_SAVED_CASE_MS

python3 - "$_SUB_TRACE5" "$_SUB_TRACE5B" <<'PY'
import json
import sys

def load_assertion_ids(path):
    ids = []
    with open(path, "r") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            ev = json.loads(line)
            aid = ev.get("assertion_id")
            if aid:
                ids.append(aid)
    return ids

ids_a = load_assertion_ids(sys.argv[1])
ids_b = load_assertion_ids(sys.argv[2])

errors = []

if ids_a != ids_b:
    errors.append(f"assertion IDs differ between runs: {ids_a} vs {ids_b}")

# Verify expected IDs
expected = ["det_case.a1", "det_case.a2", "det_case.a3"]
if ids_a != expected:
    errors.append(f"unexpected IDs: {ids_a}, expected {expected}")

if errors:
    for e in errors:
        print(f"  - {e}", file=sys.stderr)
    sys.exit(1)
PY

if [ $? -eq 0 ]; then
    e2e_pass "assertion IDs are stable and deterministic across runs"
else
    e2e_fail "assertion IDs are not deterministic"
fi

# ---------------------------------------------------------------------------
# Case 6: Backward compatibility - schema_version 1 traces still validate
# ---------------------------------------------------------------------------
e2e_case_banner "Backward compatibility with schema_version 1 traces"

COMPAT_DIR="$(e2e_mktemp "logging_contract_compat")"
mkdir -p "${COMPAT_DIR}/diagnostics" "${COMPAT_DIR}/trace" "${COMPAT_DIR}/transcript" \
         "${COMPAT_DIR}/logs" "${COMPAT_DIR}/screenshots"

# Write v1-style trace events (no assertion_id, no step, no elapsed_ms)
cat > "${COMPAT_DIR}/trace/events.jsonl" <<EOF
{"schema_version":1,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_STARTED_AT}","kind":"suite_start","case":"","message":"","counters":{"total":0,"pass":0,"fail":0,"skip":0}}
{"schema_version":1,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_STARTED_AT}","kind":"assert_pass","case":"legacy","message":"old format","counters":{"total":1,"pass":1,"fail":0,"skip":0}}
{"schema_version":1,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_ENDED_AT}","kind":"suite_end","case":"","message":"","counters":{"total":1,"pass":1,"fail":0,"skip":0}}
EOF

e2e_write_summary_json "${COMPAT_DIR}"
e2e_write_meta_json "${COMPAT_DIR}"
e2e_write_metrics_json "${COMPAT_DIR}"
e2e_write_diagnostics_files "${COMPAT_DIR}"
e2e_write_transcript_summary "${COMPAT_DIR}"
e2e_write_repro_files "${COMPAT_DIR}"
e2e_write_forensic_indexes "${COMPAT_DIR}"
e2e_write_bundle_manifest "${COMPAT_DIR}"

if e2e_validate_bundle_manifest "${COMPAT_DIR}"; then
    e2e_pass "v1 trace events still pass validation"
else
    e2e_fail "v1 trace events rejected by validator"
fi

e2e_summary
