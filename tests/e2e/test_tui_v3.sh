#!/usr/bin/env bash
# test_tui_v3.sh - Master TUI V3 E2E orchestrator (br-3mbli)
#
# Runs the three TUI V3 suites sequentially and aggregates totals:
#   - test_tui_v3_charts.sh
#   - test_tui_v3_rendering.sh
#   - test_tui_v3_interaction.sh
#
# Outputs:
#   - tests/artifacts/tui_v3/<timestamp>/*
#   - tests/artifacts_native/tui_v3/<timestamp>/* (aggregated mirror + links)

set -euo pipefail

# Safety: default to keeping temp dirs so shared harness cleanup does not run
# destructive deletion commands in constrained environments.
: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="tui_v3"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "TUI V3 Master E2E Orchestrator (br-3mbli)"

MASTER_LOG="${E2E_ARTIFACT_DIR}/run_${E2E_TIMESTAMP}.log"
SUBSUITE_RESULTS_TSV="${E2E_ARTIFACT_DIR}/subsuite_results.tsv"
SUBSUITE_RESULTS_JSONL="${E2E_ARTIFACT_DIR}/diagnostics/subsuite_results.jsonl"
AGGREGATE_SUMMARY_JSON="${E2E_ARTIFACT_DIR}/aggregate_summary.json"
NATIVE_RUN_DIR="${E2E_PROJECT_ROOT}/tests/artifacts_native/tui_v3/${E2E_TIMESTAMP}"

mkdir -p "${E2E_ARTIFACT_DIR}/logs" "${NATIVE_RUN_DIR}/logs"
: > "${MASTER_LOG}"
: > "${SUBSUITE_RESULTS_JSONL}"
{
    printf "suite_id\tscript\trc\ttotal\tpass\tfail\tskip\telapsed_ms\tartifact_dir\n"
} > "${SUBSUITE_RESULTS_TSV}"

e2e_log "Master log: ${MASTER_LOG}"
e2e_log "Native archive dir: ${NATIVE_RUN_DIR}"

AGG_TOTAL=0
AGG_PASS=0
AGG_FAIL=0
AGG_SKIP=0
ORCHESTRATOR_FAIL=0

read_summary_counts() {
    local summary_json="$1"
    python3 - "$summary_json" <<'PY'
import json
import sys

path = sys.argv[1]
try:
    with open(path, "r", encoding="utf-8") as fh:
        data = json.load(fh)
except Exception:
    print("0\t0\t0\t0")
    sys.exit(0)

def _n(value):
    try:
        return int(value)
    except Exception:
        return 0

print(
    f"{_n(data.get('total', 0))}\t{_n(data.get('pass', 0))}\t{_n(data.get('fail', 0))}\t{_n(data.get('skip', 0))}"
)
PY
}

append_subsuite_json() {
    local suite_id="$1"
    local script_name="$2"
    local rc="$3"
    local total="$4"
    local pass="$5"
    local fail="$6"
    local skip="$7"
    local elapsed_ms="$8"
    local artifact_dir="$9"

    {
        printf '{'
        printf '"schema_version":1,'
        printf '"suite":"%s",' "$(_e2e_json_escape "$E2E_SUITE")"
        printf '"subsuite_id":"%s",' "$(_e2e_json_escape "$suite_id")"
        printf '"script":"%s",' "$(_e2e_json_escape "$script_name")"
        printf '"rc":%s,' "$rc"
        printf '"elapsed_ms":%s,' "$elapsed_ms"
        printf '"total":%s,' "$total"
        printf '"pass":%s,' "$pass"
        printf '"fail":%s,' "$fail"
        printf '"skip":%s,' "$skip"
        printf '"artifact_dir":"%s"' "$(_e2e_json_escape "$artifact_dir")"
        printf '}\n'
    } >> "${SUBSUITE_RESULTS_JSONL}"
}

extract_artifact_dir_from_log() {
    local log_file="$1"
    sed -nE 's/^[[:space:]]*Artifacts:[[:space:]]*(.*)$/\1/p' "${log_file}" | tail -n 1
}

run_subsuite() {
    local suite_id="$1"
    local script_name="$2"
    local script_path="${SCRIPT_DIR}/${script_name}"
    local log_file="${E2E_ARTIFACT_DIR}/logs/${suite_id}.log"
    local native_log_file="${NATIVE_RUN_DIR}/logs/${suite_id}.log"

    e2e_case_banner "${suite_id}"
    e2e_log "subsuite script: ${script_path}"

    if [ ! -f "${script_path}" ]; then
        e2e_fail "${suite_id}: script missing (${script_path})"
        ORCHESTRATOR_FAIL=1
        return 0
    fi

    local start_ms end_ms elapsed_ms rc
    start_ms="$(_e2e_now_ms)"
    set +e
    bash "${script_path}" >"${log_file}" 2>&1
    rc=$?
    set -e
    end_ms="$(_e2e_now_ms)"
    elapsed_ms=$((end_ms - start_ms))

    cp "${log_file}" "${native_log_file}"
    {
        echo ""
        echo "===== ${suite_id} (${script_name}) ====="
        cat "${log_file}"
    } >> "${MASTER_LOG}"

    local artifact_dir summary_json
    artifact_dir="$(extract_artifact_dir_from_log "${log_file}")"
    summary_json=""
    if [ -n "${artifact_dir}" ] && [ -f "${artifact_dir}/summary.json" ]; then
        summary_json="${artifact_dir}/summary.json"
    fi

    local total pass fail skip
    total=0
    pass=0
    fail=0
    skip=0

    if [ -n "${summary_json}" ]; then
        IFS=$'\t' read -r total pass fail skip < <(read_summary_counts "${summary_json}")
    fi

    AGG_TOTAL=$((AGG_TOTAL + total))
    AGG_PASS=$((AGG_PASS + pass))
    AGG_FAIL=$((AGG_FAIL + fail))
    AGG_SKIP=$((AGG_SKIP + skip))

    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
        "${suite_id}" "${script_name}" "${rc}" "${total}" "${pass}" "${fail}" "${skip}" "${elapsed_ms}" "${artifact_dir:-unknown}" \
        >> "${SUBSUITE_RESULTS_TSV}"
    append_subsuite_json "${suite_id}" "${script_name}" "${rc}" "${total}" "${pass}" "${fail}" "${skip}" "${elapsed_ms}" "${artifact_dir:-unknown}"

    if [ -n "${artifact_dir}" ] && [ -d "${artifact_dir}" ]; then
        ln -sfn "${artifact_dir}" "${NATIVE_RUN_DIR}/${suite_id}" || true
    fi

    if [ "${rc}" -ne 0 ] || [ "${fail}" -gt 0 ]; then
        e2e_fail "${suite_id}: rc=${rc}, fail=${fail}, pass=${pass}, skip=${skip}"
        ORCHESTRATOR_FAIL=1
    else
        e2e_pass "${suite_id}: rc=${rc}, fail=${fail}, pass=${pass}, skip=${skip}"
    fi
}

run_subsuite "case01_tui_v3_charts" "test_tui_v3_charts.sh"
run_subsuite "case02_tui_v3_rendering" "test_tui_v3_rendering.sh"
run_subsuite "case03_tui_v3_interaction" "test_tui_v3_interaction.sh"

{
    printf '{\n'
    printf '  "schema_version": 1,\n'
    printf '  "suite": "%s",\n' "$E2E_SUITE"
    printf '  "timestamp": "%s",\n' "$E2E_TIMESTAMP"
    printf '  "total": %s,\n' "$AGG_TOTAL"
    printf '  "pass": %s,\n' "$AGG_PASS"
    printf '  "fail": %s,\n' "$AGG_FAIL"
    printf '  "skip": %s,\n' "$AGG_SKIP"
    printf '  "subsuite_report_tsv": "%s",\n' "subsuite_results.tsv"
    printf '  "subsuite_report_jsonl": "%s",\n' "diagnostics/subsuite_results.jsonl"
    printf '  "master_log": "%s"\n' "run_${E2E_TIMESTAMP}.log"
    printf '}\n'
} > "${AGGREGATE_SUMMARY_JSON}"

cp "${AGGREGATE_SUMMARY_JSON}" "${NATIVE_RUN_DIR}/aggregate_summary.json"
cp "${SUBSUITE_RESULTS_TSV}" "${NATIVE_RUN_DIR}/subsuite_results.tsv"
cp "${SUBSUITE_RESULTS_JSONL}" "${NATIVE_RUN_DIR}/subsuite_results.jsonl"
cp "${MASTER_LOG}" "${NATIVE_RUN_DIR}/run_${E2E_TIMESTAMP}.log"

e2e_save_artifact "subsuite_results.tsv" "$(cat "${SUBSUITE_RESULTS_TSV}")"
e2e_save_artifact "aggregate_summary.json" "$(cat "${AGGREGATE_SUMMARY_JSON}")"

e2e_log "aggregate totals: total=${AGG_TOTAL} pass=${AGG_PASS} fail=${AGG_FAIL} skip=${AGG_SKIP}"

if [ "${ORCHESTRATOR_FAIL}" -ne 0 ] || [ "${AGG_FAIL}" -gt 0 ]; then
    e2e_fail "orchestrator aggregate contains failures"
else
    e2e_pass "orchestrator aggregate has zero failures"
fi

e2e_summary
