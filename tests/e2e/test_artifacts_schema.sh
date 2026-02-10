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
{"schema_version":1,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_STARTED_AT}","kind":"suite_start","case":"","message":"","counters":{"total":0,"pass":0,"fail":0,"skip":0}}
{"schema_version":1,"suite":"${E2E_SUITE}","run_timestamp":"${E2E_TIMESTAMP}","ts":"${E2E_RUN_ENDED_AT}","kind":"suite_end","case":"","message":"","counters":{"total":0,"pass":0,"fail":0,"skip":0}}
EOF

e2e_write_summary_json "${GOOD_DIR}"
e2e_write_meta_json "${GOOD_DIR}"
e2e_write_metrics_json "${GOOD_DIR}"
e2e_write_diagnostics_files "${GOOD_DIR}"
e2e_write_transcript_summary "${GOOD_DIR}"
e2e_write_bundle_manifest "${GOOD_DIR}"

e2e_case_banner "Valid bundle validates"
if e2e_validate_bundle_manifest "${GOOD_DIR}"; then
    e2e_pass "validator accepts good bundle"
else
    e2e_fail "validator rejected good bundle"
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

e2e_summary

