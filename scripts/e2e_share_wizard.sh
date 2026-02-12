#!/usr/bin/env bash
# e2e_share_wizard.sh - Native share wizard E2E suite (br-18tuh)
#
# Covers:
#   1) Non-interactive success path with JSON output
#   2) Non-TTY invocation behavior
#   3) Missing-provider failure in isolated cwd (no detection signals)
#   4) Invalid flag handling (clap exit semantics)
#   5) Partial-config failure (bundle missing manifest.json)
#
# Artifacts:
#   tests/artifacts/share_wizard/<timestamp>/
#   ├── cases/<case_id>/{command.txt,stdout.txt,stderr.txt,status.txt,timing_ms.txt}
#   ├── transcript/commands.log
#   ├── diagnostics/case_timings.tsv
#   ├── diagnostics/cases.jsonl
#   └── wizard_case_summary.json

set -euo pipefail

E2E_SUITE="share_wizard"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Share Wizard E2E Suite (br-18tuh)"

AM_BIN="$(e2e_ensure_binary "am" | tail -n 1)"
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: ${AM_BIN}"

WORK="$(e2e_mktemp "e2e_share_wizard")"
BUNDLE_OK="${WORK}/bundle_ok"
BUNDLE_PARTIAL="${WORK}/bundle_partial"
ISOLATED_CWD="${WORK}/isolated_cwd"
mkdir -p "${BUNDLE_OK}" "${BUNDLE_PARTIAL}" "${ISOLATED_CWD}"
echo '{}' > "${BUNDLE_OK}/manifest.json"

COMMAND_LOG="${E2E_ARTIFACT_DIR}/transcript/commands.log"
TIMINGS_TSV="${E2E_ARTIFACT_DIR}/diagnostics/case_timings.tsv"
CASES_JSONL="${E2E_ARTIFACT_DIR}/diagnostics/cases.jsonl"
printf "case_id\texpected_rc\tactual_rc\tduration_ms\n" > "${TIMINGS_TSV}"
: > "${CASES_JSONL}"
: > "${COMMAND_LOG}"

run_case() {
    local case_id="$1"
    local expected_rc="$2"
    local command="$3"
    local case_dir="${E2E_ARTIFACT_DIR}/cases/${case_id}"
    mkdir -p "${case_dir}"

    e2e_case_banner "${case_id}"
    e2e_log "command: ${command}"
    printf '%s\n' "${command}" > "${case_dir}/command.txt"
    printf '[%s] %s\n' "$(date -u '+%Y-%m-%dT%H:%M:%SZ')" "${command}" >> "${COMMAND_LOG}"

    local start_ns end_ns duration_ms rc
    start_ns="$(date +%s%N)"
    set +e
    bash -lc "${command}" > "${case_dir}/stdout.txt" 2> "${case_dir}/stderr.txt"
    rc=$?
    set -e
    end_ns="$(date +%s%N)"
    duration_ms="$(( (end_ns - start_ns) / 1000000 ))"

    printf '%s\n' "${rc}" > "${case_dir}/status.txt"
    printf '%s\n' "${duration_ms}" > "${case_dir}/timing_ms.txt"
    printf '%s\t%s\t%s\t%s\n' "${case_id}" "${expected_rc}" "${rc}" "${duration_ms}" >> "${TIMINGS_TSV}"

    e2e_assert_exit_code "${case_id} exit code" "${expected_rc}" "${rc}"

    python3 - "${CASES_JSONL}" "${case_id}" "${expected_rc}" "${rc}" "${duration_ms}" <<'PY'
import json
import sys

path, case_id, expected_rc, actual_rc, duration_ms = sys.argv[1:]
entry = {
    "case_id": case_id,
    "expected_rc": int(expected_rc),
    "actual_rc": int(actual_rc),
    "duration_ms": int(duration_ms),
    "passed": int(expected_rc) == int(actual_rc),
}
with open(path, "a", encoding="utf-8") as fh:
    fh.write(json.dumps(entry, sort_keys=True) + "\n")
PY
}

assert_json_field_eq() {
    local label="$1"
    local file="$2"
    local key="$3"
    local expected="$4"
    local actual
    actual="$(python3 - "${file}" "${key}" <<'PY'
import json
import sys

path, key = sys.argv[1:]
value = json.load(open(path, encoding="utf-8"))
for part in key.split("."):
    if isinstance(value, dict):
        value = value.get(part)
    else:
        value = None
        break
if value is None:
    print("")
elif isinstance(value, bool):
    print(str(value).lower())
else:
    print(value)
PY
)"
    e2e_assert_eq "${label}" "${expected}" "${actual}"
}

# ---------------------------------------------------------------------------
# Case 1: Non-interactive success (JSON + dry-run)
# ---------------------------------------------------------------------------
printf -v CMD_01 '%q share wizard --bundle %q --provider custom --non-interactive --yes --dry-run --json' \
    "${AM_BIN}" "${BUNDLE_OK}"
run_case "case_01_non_interactive_success" 0 "${CMD_01}"
assert_json_field_eq \
    "case_01 provider is custom" \
    "${E2E_ARTIFACT_DIR}/cases/case_01_non_interactive_success/stdout.txt" \
    "provider" \
    "custom"
assert_json_field_eq \
    "case_01 top-level success true" \
    "${E2E_ARTIFACT_DIR}/cases/case_01_non_interactive_success/stdout.txt" \
    "success" \
    "true"

# ---------------------------------------------------------------------------
# Case 2: Non-TTY path still runs deterministically
# ---------------------------------------------------------------------------
printf -v CMD_02 '%q share wizard --bundle %q --provider custom --yes --dry-run --json < /dev/null' \
    "${AM_BIN}" "${BUNDLE_OK}"
run_case "case_02_non_tty_success" 0 "${CMD_02}"
assert_json_field_eq \
    "case_02 mode is non_interactive" \
    "${E2E_ARTIFACT_DIR}/cases/case_02_non_tty_success/stdout.txt" \
    "result.metadata.mode" \
    "non_interactive"

# ---------------------------------------------------------------------------
# Case 3: Missing provider in isolated cwd (no detection signals)
# ---------------------------------------------------------------------------
printf -v CMD_03 'cd %q && %q share wizard --bundle %q --non-interactive --yes --dry-run --json' \
    "${ISOLATED_CWD}" "${AM_BIN}" "${BUNDLE_OK}"
run_case "case_03_missing_provider_failure" 1 "${CMD_03}"
assert_json_field_eq \
    "case_03 error_code missing required option" \
    "${E2E_ARTIFACT_DIR}/cases/case_03_missing_provider_failure/stdout.txt" \
    "error_code" \
    "MISSING_REQUIRED_OPTION"

# ---------------------------------------------------------------------------
# Case 4: Invalid flag exits 2 with clap usage error
# ---------------------------------------------------------------------------
printf -v CMD_04 '%q share wizard --bogus' "${AM_BIN}"
run_case "case_04_invalid_flag" 2 "${CMD_04}"
e2e_assert_contains \
    "case_04 stderr mentions unexpected argument" \
    "$(cat "${E2E_ARTIFACT_DIR}/cases/case_04_invalid_flag/stderr.txt")" \
    "unexpected argument '--bogus'"

# ---------------------------------------------------------------------------
# Case 5: Partial config state (bundle missing manifest)
# ---------------------------------------------------------------------------
printf -v CMD_05 '%q share wizard --bundle %q --provider custom --non-interactive --yes --dry-run --json' \
    "${AM_BIN}" "${BUNDLE_PARTIAL}"
run_case "case_05_bundle_missing_manifest" 1 "${CMD_05}"
assert_json_field_eq \
    "case_05 error_code bundle invalid" \
    "${E2E_ARTIFACT_DIR}/cases/case_05_bundle_missing_manifest/stdout.txt" \
    "error_code" \
    "BUNDLE_INVALID"

# ---------------------------------------------------------------------------
# Build deterministic per-case summary
# ---------------------------------------------------------------------------
python3 - "${CASES_JSONL}" "${E2E_ARTIFACT_DIR}/wizard_case_summary.json" <<'PY'
import json
import sys

jsonl_path, summary_path = sys.argv[1:]
cases = []
with open(jsonl_path, encoding="utf-8") as fh:
    for line in fh:
        line = line.strip()
        if not line:
            continue
        cases.append(json.loads(line))

summary = {
    "schema_version": "share_wizard_e2e.v1",
    "total_cases": len(cases),
    "passed": sum(1 for c in cases if c.get("passed")),
    "failed": sum(1 for c in cases if not c.get("passed")),
    "cases": cases,
}

with open(summary_path, "w", encoding="utf-8") as fh:
    json.dump(summary, fh, indent=2, sort_keys=True)
    fh.write("\n")
PY

e2e_assert_file_exists \
    "wizard_case_summary.json exists" \
    "${E2E_ARTIFACT_DIR}/wizard_case_summary.json"

if [ "${AM_E2E_KEEP_TMP:-0}" != "1" ]; then
    rm -rf "${WORK}"
fi

e2e_summary
