#!/usr/bin/env bash
# test_artifacts_schema.sh - Artifact bundle schema + validator self-test (br-3vwi.10.18)
#
# Verifies:
# - A valid bundle passes validation
# - Forward-compatible evolution (minor bump + extra keys) is accepted
# - Malformed bundles (bad major / missing required refs / bytes mismatch) fail

E2E_SUITE="artifacts_schema"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Artifact Bundle Schema Validator E2E Suite (br-3vwi.10.18)"

if ! command -v python3 >/dev/null 2>&1; then
    e2e_log "python3 not found; skipping suite"
    e2e_skip "python3 required for strict validation"
    e2e_summary
    exit 0
fi

FIX_ROOT="$(e2e_mktemp "e2e_artifacts_schema")"
GOOD_DIR="${FIX_ROOT}/good"

mkdir -p "${GOOD_DIR}/diagnostics" "${GOOD_DIR}/trace" "${GOOD_DIR}/transcript"

# Populate required typed artifacts in the fixture dir.
E2E_RUN_ENDED_AT="$(_e2e_now_rfc3339)"
E2E_RUN_END_EPOCH_S="$(date +%s)"

cat > "${GOOD_DIR}/trace/events.jsonl" <<EOF
{"schema_version":2,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_STARTED_AT}","kind":"suite_start","case":"","message":"","counters":{"total":0,"pass":0,"fail":0,"skip":0}}
{"schema_version":2,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_STARTED_AT}","kind":"case_start","case":"scenario_fixture","message":"","counters":{"total":0,"pass":0,"fail":0,"skip":0}}
{"schema_version":2,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_STARTED_AT}","kind":"assert_pass","case":"scenario_fixture","message":"fixture assertion","counters":{"total":1,"pass":1,"fail":0,"skip":0},"assertion_id":"scenario_fixture.a1","elapsed_ms":7}
{"schema_version":2,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_ENDED_AT}","kind":"case_end","case":"scenario_fixture","message":"","counters":{"total":1,"pass":1,"fail":0,"skip":0},"elapsed_ms":7}
{"schema_version":2,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_ENDED_AT}","kind":"suite_end","case":"","message":"","counters":{"total":1,"pass":1,"fail":0,"skip":0}}
EOF

e2e_write_summary_json "${GOOD_DIR}"
e2e_write_meta_json "${GOOD_DIR}"
e2e_write_metrics_json "${GOOD_DIR}"
e2e_write_diagnostics_files "${GOOD_DIR}"
e2e_write_transcript_summary "${GOOD_DIR}"
e2e_write_repro_files "${GOOD_DIR}"
e2e_write_forensic_indexes "${GOOD_DIR}"
e2e_write_suite_manifest_json "${GOOD_DIR}"
e2e_write_scenario_logs "${GOOD_DIR}"
e2e_write_bundle_manifest "${GOOD_DIR}"

e2e_case_banner "Valid bundle validates"
if e2e_validate_bundle_manifest "${GOOD_DIR}"; then
    e2e_pass "validator accepts good bundle"
else
    e2e_fail "validator rejected good bundle"
fi

e2e_case_banner "Scenario replay logs are emitted"
if python3 - <<'PY' "${GOOD_DIR}/scenarios/index.json" "${GOOD_DIR}/scenarios/scenarios.jsonl"
import json
import sys

index_path, jsonl_path = sys.argv[1:3]
with open(index_path, "r", encoding="utf-8") as f:
    index = json.load(f)
with open(jsonl_path, "r", encoding="utf-8") as f:
    scenarios = [json.loads(line) for line in f if line.strip()]

assert index["schema_version"] == 1
assert index["scenario_count"] == 1
assert index["scenarios_jsonl"] == "scenarios/scenarios.jsonl"
assert index["replay"]["command"]
assert index["diagnostics"]["env_redacted"] == "diagnostics/env_redacted.txt"
assert len(scenarios) == 1
scenario = scenarios[0]
assert scenario["case"] == "scenario_fixture"
assert scenario["status"] == "pass"
assert scenario["assertion_count"] == 1
assert scenario["trace"]["events"] == "trace/events.jsonl"
assert scenario["replay"]["command"]
assert scenario["replay"]["environment"] == "repro.env"
assert scenario["diagnostics"]["env_redacted"] == "diagnostics/env_redacted.txt"
assert isinstance(scenario["artifacts"], list)
assert isinstance(scenario["failures"], list)
PY
then
    e2e_pass "scenario replay index and JSONL are complete"
else
    e2e_fail "scenario replay log shape is incomplete"
fi

e2e_case_banner "Expected/actual diff artifacts feed scenario replay"
DIFF_DIR="${FIX_ROOT}/diff_fixture"
mkdir -p "${DIFF_DIR}/diagnostics" "${DIFF_DIR}/trace" "${DIFF_DIR}/transcript" \
         "${DIFF_DIR}/logs" "${DIFF_DIR}/screenshots"

SAVED_ARTIFACT_DIR="${E2E_ARTIFACT_DIR}"
SAVED_TRACE_FILE="${_E2E_TRACE_FILE}"
SAVED_CASE_ARTIFACTS_FILE="${_E2E_CASE_ARTIFACTS_FILE}"
SAVED_CURRENT_CASE="${_E2E_CURRENT_CASE}"
SAVED_MARKER_CASE="${_E2E_MARKER_ACTIVE_CASE}"

E2E_ARTIFACT_DIR="${DIFF_DIR}"
_E2E_TRACE_FILE="${DIFF_DIR}/trace/events.jsonl"
_E2E_CASE_ARTIFACTS_FILE="${DIFF_DIR}/trace/.case_artifacts.tsv"
_E2E_CURRENT_CASE="diff_fixture_case"
_E2E_MARKER_ACTIVE_CASE="diff_fixture_case"
: >"${_E2E_TRACE_FILE}"
: >"${_E2E_CASE_ARTIFACTS_FILE}"
_e2e_trace_event "suite_start" ""
_e2e_trace_event "case_start" "" "diff_fixture_case"
_e2e_trace_event "assert_fail" "fixture mismatch" "diff_fixture_case" "diff_fixture_case.a1" "" "3"
e2e_diff "fixture mismatch" "expected value" "actual value"
_e2e_trace_event "case_end" "" "diff_fixture_case" "" "" "3"
_e2e_trace_event "suite_end" ""

E2E_RUN_ENDED_AT="$(_e2e_now_rfc3339)"
E2E_RUN_END_EPOCH_S="$(date +%s)"
e2e_write_summary_json "${DIFF_DIR}"
e2e_write_meta_json "${DIFF_DIR}"
e2e_write_metrics_json "${DIFF_DIR}"
e2e_write_diagnostics_files "${DIFF_DIR}"
e2e_write_transcript_summary "${DIFF_DIR}"
e2e_write_repro_files "${DIFF_DIR}"
e2e_write_forensic_indexes "${DIFF_DIR}"
e2e_write_suite_manifest_json "${DIFF_DIR}"
e2e_write_scenario_logs "${DIFF_DIR}"
e2e_write_bundle_manifest "${DIFF_DIR}"

E2E_ARTIFACT_DIR="${SAVED_ARTIFACT_DIR}"
_E2E_TRACE_FILE="${SAVED_TRACE_FILE}"
_E2E_CASE_ARTIFACTS_FILE="${SAVED_CASE_ARTIFACTS_FILE}"
_E2E_CURRENT_CASE="${SAVED_CURRENT_CASE}"
_E2E_MARKER_ACTIVE_CASE="${SAVED_MARKER_CASE}"

if e2e_validate_bundle_manifest "${DIFF_DIR}" && python3 - <<'PY' "${DIFF_DIR}/scenarios/scenarios.jsonl"
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as f:
    scenarios = [json.loads(line) for line in f if line.strip()]
assert len(scenarios) == 1
diffs = [f for f in scenarios[0]["failures"] if f.get("artifact")]
assert diffs
diff = diffs[0]
assert diff["artifact"] == "failures/fail_1.json"
assert diff["label"] == "fixture mismatch"
assert diff["expected"] == "expected value"
assert diff["actual"] == "actual value"
assert diff["replay_command"]
assert diff["env_redacted_path"] == "diagnostics/env_redacted.txt"
PY
then
    e2e_pass "expected/actual failure artifact is replay-linked"
else
    e2e_fail "expected/actual failure artifact missing from scenario replay"
fi

e2e_case_banner "Minor bump + extra keys are accepted"
EVOLVED_DIR="${FIX_ROOT}/evolved"
cp -r "${GOOD_DIR}" "${EVOLVED_DIR}"
python3 - <<'PY' "${EVOLVED_DIR}/bundle.json"
import json
import sys

path = sys.argv[1]
with open(path, "r", encoding="utf-8") as f:
    d = json.load(f)

d["schema"]["minor"] = 999
d["extra_top_level"] = {"note": "forward-compatible addition"}
d["artifacts"]["extra_ref"] = {"path": "extra.txt"}

with open(path, "w", encoding="utf-8") as f:
    json.dump(d, f, indent=2, sort_keys=True)
    f.write("\n")
PY
if e2e_validate_bundle_manifest "${EVOLVED_DIR}"; then
    e2e_pass "validator accepts minor bump + extra keys"
else
    e2e_fail "validator rejected compatible evolution"
fi

e2e_case_banner "Bad major is rejected (negative test)"
BAD_MAJOR_DIR="${FIX_ROOT}/bad_major"
cp -r "${GOOD_DIR}" "${BAD_MAJOR_DIR}"
python3 - <<'PY' "${BAD_MAJOR_DIR}/bundle.json"
import json
import sys

path = sys.argv[1]
with open(path, "r", encoding="utf-8") as f:
    d = json.load(f)
d["schema"]["major"] = 2
with open(path, "w", encoding="utf-8") as f:
    json.dump(d, f, indent=2, sort_keys=True)
    f.write("\n")
PY
if e2e_validate_bundle_manifest "${BAD_MAJOR_DIR}"; then
    e2e_fail "validator accepted bad major"
else
    e2e_pass "validator rejects bad major"
fi

e2e_case_banner "Missing required artifact ref is rejected (negative test)"
BAD_MISSING_DIR="${FIX_ROOT}/bad_missing_ref"
cp -r "${GOOD_DIR}" "${BAD_MISSING_DIR}"
python3 - <<'PY' "${BAD_MISSING_DIR}/bundle.json"
import json
import sys

path = sys.argv[1]
with open(path, "r", encoding="utf-8") as f:
    d = json.load(f)
d["artifacts"]["metrics"]["path"] = "metrics_missing.json"
with open(path, "w", encoding="utf-8") as f:
    json.dump(d, f, indent=2, sort_keys=True)
    f.write("\n")
PY
if e2e_validate_bundle_manifest "${BAD_MISSING_DIR}"; then
    e2e_fail "validator accepted missing required ref"
else
    e2e_pass "validator rejects missing required ref"
fi

e2e_case_banner "Bytes mismatch is rejected (negative test)"
BAD_BYTES_DIR="${FIX_ROOT}/bad_bytes"
cp -r "${GOOD_DIR}" "${BAD_BYTES_DIR}"
python3 - <<'PY' "${BAD_BYTES_DIR}/bundle.json"
import json
import sys

path = sys.argv[1]
with open(path, "r", encoding="utf-8") as f:
    d = json.load(f)
for ent in d.get("files", []):
    if ent.get("path") == "summary.json" and isinstance(ent.get("bytes"), int):
        ent["bytes"] = ent["bytes"] + 1
        break
with open(path, "w", encoding="utf-8") as f:
    json.dump(d, f, indent=2, sort_keys=True)
    f.write("\n")
PY
if e2e_validate_bundle_manifest "${BAD_BYTES_DIR}"; then
    e2e_fail "validator accepted bytes mismatch"
else
    e2e_pass "validator rejects bytes mismatch"
fi

e2e_case_banner "Scenario artifact references are enforced (negative test)"
BAD_SCENARIO_DIR="${FIX_ROOT}/bad_scenario_ref"
cp -r "${GOOD_DIR}" "${BAD_SCENARIO_DIR}"
python3 - <<'PY' "${BAD_SCENARIO_DIR}/scenarios/scenarios.jsonl" "${BAD_SCENARIO_DIR}/bundle.json"
import hashlib
import json
import os
import sys

path, bundle_path = sys.argv[1:3]
with open(path, "r", encoding="utf-8") as f:
    scenarios = [json.loads(line) for line in f if line.strip()]
scenarios[0]["artifacts"] = [{"path": "missing-scenario-artifact.json"}]
with open(path, "w", encoding="utf-8") as f:
    for scenario in scenarios:
        json.dump(scenario, f, sort_keys=True)
        f.write("\n")
with open(bundle_path, "r", encoding="utf-8") as f:
    bundle = json.load(f)
for ent in bundle["files"]:
    if ent["path"] == "scenarios/scenarios.jsonl":
        with open(path, "rb") as scenario_file:
            ent["sha256"] = hashlib.sha256(scenario_file.read()).hexdigest()
        ent["bytes"] = os.path.getsize(path)
        break
with open(bundle_path, "w", encoding="utf-8") as f:
    json.dump(bundle, f, indent=2, sort_keys=True)
    f.write("\n")
PY
if e2e_validate_bundle_manifest "${BAD_SCENARIO_DIR}"; then
    e2e_fail "validator accepted scenario reference to missing file"
else
    e2e_pass "validator rejects scenario reference to missing file"
fi

e2e_case_banner "Manifest artifact references are enforced (negative test)"
BAD_MANIFEST_DIR="${FIX_ROOT}/bad_manifest_ref"
cp -r "${GOOD_DIR}" "${BAD_MANIFEST_DIR}"
python3 - <<'PY' "${BAD_MANIFEST_DIR}/manifest.json"
import json
import sys

path = sys.argv[1]
with open(path, "r", encoding="utf-8") as f:
    d = json.load(f)
d["cases"] = [
    {
        "name": "case_missing_artifact",
        "status": "pass",
        "duration_ms": 7,
        "assertion_count": 1,
        "artifacts": ["missing.json"],
    }
]
with open(path, "w", encoding="utf-8") as f:
    json.dump(d, f, indent=2, sort_keys=True)
    f.write("\n")
PY
e2e_write_bundle_manifest "${BAD_MANIFEST_DIR}"
if e2e_validate_bundle_manifest "${BAD_MANIFEST_DIR}"; then
    e2e_fail "validator accepted manifest artifact reference to missing file"
else
    e2e_pass "validator rejects manifest artifact reference to missing file"
fi

e2e_summary
