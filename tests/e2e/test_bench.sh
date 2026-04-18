#!/usr/bin/env bash
# test_bench.sh - E2E test suite for `am bench` (br-2x5p4)
#
# Covers:
# - list/filter JSON contracts
# - quick/full benchmark runs
# - baseline save/load comparison flows
# - regression exit behavior
# - ATC perf gate policy (relative overhead is blocking; absolute drift is advisory)

set -euo pipefail

E2E_SUITE="bench"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

e2e_init_artifacts
e2e_banner "am bench E2E Suite"

# Always rebuild for this suite so we do not validate against a stale `am` binary.
export E2E_FORCE_BUILD="${E2E_FORCE_BUILD:-1}"

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq not found; skipping bench e2e suite"
    e2e_summary
    exit 0
fi

AM_BIN="${CARGO_TARGET_DIR}/debug/am"
BUILD_LOG="${E2E_ARTIFACT_DIR}/diagnostics/build_am.log"

set +e
e2e_run_cargo build -p mcp-agent-mail-cli --bin am >"${BUILD_LOG}" 2>&1
build_rc=$?
set -e
if [ "$build_rc" -ne 0 ] || [ ! -x "$AM_BIN" ]; then
    e2e_fail "Failed to build am binary for bench E2E"
    tail -n 80 "${BUILD_LOG}" >&2 || true
    e2e_summary
    exit 1
fi

help_text="$("${AM_BIN}" --help 2>&1 || true)"
if [[ "${help_text}" != *"bench"* ]]; then
    e2e_fail "Built am binary does not expose bench subcommand"
    e2e_save_artifact "diagnostics/help_output.txt" "${help_text}"
    e2e_summary
    exit 1
fi

run_bench_case() {
    local case_id="$1"
    shift
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    mkdir -p "${case_dir}"

    e2e_mark_case_start "${case_id}"
    set +e
    "${AM_BIN}" bench "$@" >"${case_dir}/stdout.json" 2>"${case_dir}/stderr.txt"
    local rc=$?
    set -e
    echo "${rc}" >"${case_dir}/exit_code.txt"
    e2e_mark_case_end "${case_id}"
    return "${rc}"
}

WORKDIR="$(e2e_mktemp bench-e2e)"
pushd "${WORKDIR}" >/dev/null

# ---------------------------------------------------------------------------
# Case 1: --list --json --filter help
# ---------------------------------------------------------------------------
e2e_case_banner "bench_list_filter_json"
if run_bench_case "bench_list_filter_json" --list --json --filter help; then
    out_file="${E2E_ARTIFACT_DIR}/bench_list_filter_json/stdout.json"
    count="$(jq -r 'length' "${out_file}")"
    name="$(jq -r '.[0].name' "${out_file}")"
    warmup="$(jq -r '.[0].warmup' "${out_file}")"
    runs="$(jq -r '.[0].runs' "${out_file}")"

    e2e_assert_eq "list output has one benchmark" "1" "${count}"
    e2e_assert_eq "list output benchmark is help" "help" "${name}"
    e2e_assert_eq "list output default warmup" "3" "${warmup}"
    e2e_assert_eq "list output default runs" "10" "${runs}"

    report_count="$(find benches/results -maxdepth 1 -name 'summary_*.json' 2>/dev/null | wc -l | tr -d ' ')"
    e2e_assert_eq "list mode does not write summary report" "0" "${report_count}"
else
    e2e_fail "bench --list --json --filter help failed"
fi

# ---------------------------------------------------------------------------
# Case 2: --quick --json --filter help
# ---------------------------------------------------------------------------
e2e_case_banner "bench_quick_help_json"
if run_bench_case "bench_quick_help_json" --quick --json --filter help; then
    out_file="${E2E_ARTIFACT_DIR}/bench_quick_help_json/stdout.json"
    warmup="$(jq -r '.warmup' "${out_file}")"
    runs="$(jq -r '.runs' "${out_file}")"
    schema="$(jq -r '.summary.schema_version' "${out_file}")"
    has_help="$(jq -r '.summary.benchmarks.help != null' "${out_file}")"
    fixture_sig="$(jq -r '.summary.benchmarks.help.fixture_signature' "${out_file}")"

    e2e_assert_eq "quick mode warmup" "1" "${warmup}"
    e2e_assert_eq "quick mode runs" "3" "${runs}"
    e2e_assert_eq "summary schema_version" "1" "${schema}"
    e2e_assert_eq "help benchmark exists in summary" "true" "${has_help}"

    if [[ "${fixture_sig}" =~ ^[0-9a-f]{16}$ ]]; then
        e2e_pass "fixture signature is 16-char lowercase hex"
    else
        e2e_fail "fixture signature format invalid: ${fixture_sig}"
    fi

    report_count="$(find benches/results -maxdepth 1 -name 'summary_*.json' 2>/dev/null | wc -l | tr -d ' ')"
    if [ "${report_count}" -ge 1 ]; then
        e2e_pass "quick run writes summary report"
    else
        e2e_fail "quick run did not write summary report"
    fi
else
    e2e_fail "bench --quick --json --filter help failed"
fi

# ---------------------------------------------------------------------------
# Case 2b: ATC-mode mail_send benchmarks expose env wiring
# ---------------------------------------------------------------------------
e2e_case_banner "bench_list_atc_mail_send_json"
if run_bench_case "bench_list_atc_mail_send_json" --list --json --filter "mail_send*"; then
    out_file="${E2E_ARTIFACT_DIR}/bench_list_atc_mail_send_json/stdout.json"
    no_atc="$(jq -r '.[] | select(.name=="mail_send_no_atc") | .env.ATC_LEARNING_DISABLED' "${out_file}")"
    shadow="$(jq -r '.[] | select(.name=="mail_send_atc_shadow") | .env.AM_ATC_WRITE_MODE' "${out_file}")"
    live="$(jq -r '.[] | select(.name=="mail_send_atc_live") | .env.AM_ATC_WRITE_MODE' "${out_file}")"

    e2e_assert_eq "mail_send_no_atc disables ATC" "1" "${no_atc}"
    e2e_assert_eq "mail_send_atc_shadow sets shadow mode" "shadow" "${shadow}"
    e2e_assert_eq "mail_send_atc_live sets live mode" "live" "${live}"
else
    e2e_fail "bench --list --json --filter mail_send* failed"
fi

# ---------------------------------------------------------------------------
# Case 3: full --json run
# ---------------------------------------------------------------------------
e2e_case_banner "bench_full_json"
if run_bench_case "bench_full_json" --json; then
    out_file="${E2E_ARTIFACT_DIR}/bench_full_json/stdout.json"
    bench_count="$(jq -r '.summary.benchmarks | length' "${out_file}")"
    host="$(jq -r '.summary.hardware.hostname' "${out_file}")"
    arch="$(jq -r '.summary.hardware.arch' "${out_file}")"
    kernel="$(jq -r '.summary.hardware.kernel' "${out_file}")"

    if [ "${bench_count}" -ge 10 ]; then
        e2e_pass "full run executes substantial benchmark set (${bench_count})"
    else
        e2e_fail "full run benchmark count too low (${bench_count})"
    fi

    if [ -n "${host}" ] && [ -n "${arch}" ] && [ -n "${kernel}" ]; then
        e2e_pass "hardware metadata populated"
    else
        e2e_fail "hardware metadata missing host/arch/kernel"
    fi
else
    e2e_fail "bench --json full run failed"
fi

# ---------------------------------------------------------------------------
# Case 4: baseline save + compare
# ---------------------------------------------------------------------------
e2e_case_banner "bench_baseline_save_and_compare"
BASELINE_PATH="${WORKDIR}/help_baseline.json"
if run_bench_case "bench_save_baseline" --json --filter help --save-baseline "${BASELINE_PATH}"; then
    e2e_assert_file_exists "baseline file created" "${BASELINE_PATH}"
    baseline_keys="$(jq -r 'keys | join(",")' "${BASELINE_PATH}")"
    e2e_assert_contains "baseline includes help key" "${baseline_keys}" "help"
else
    e2e_fail "bench --save-baseline failed"
fi

if run_bench_case "bench_compare_saved_baseline" --json --filter help --baseline "${BASELINE_PATH}"; then
    out_file="${E2E_ARTIFACT_DIR}/bench_compare_saved_baseline/stdout.json"
    baseline_present="$(jq -r '.summary.benchmarks.help.baseline.baseline_p95_ms != null' "${out_file}")"
    delta_present="$(jq -r '.summary.benchmarks.help.baseline.delta_p95_ms != null' "${out_file}")"
    e2e_assert_eq "baseline compare populates baseline_p95_ms" "true" "${baseline_present}"
    e2e_assert_eq "baseline compare populates delta_p95_ms" "true" "${delta_present}"
else
    e2e_fail "bench --baseline compare failed"
fi

# ---------------------------------------------------------------------------
# Case 5: forced regression exits with code 3
# ---------------------------------------------------------------------------
e2e_case_banner "bench_forced_regression_exit_code"
FORCED_BASELINE="${WORKDIR}/forced_regression_baseline.json"
cat > "${FORCED_BASELINE}" <<'JSON'
{"help":0.01}
JSON

set +e
run_bench_case "bench_forced_regression" --json --filter help --baseline "${FORCED_BASELINE}"
reg_rc=$?
set -e

e2e_assert_eq "forced regression returns exit code 3" "3" "${reg_rc}"
out_file="${E2E_ARTIFACT_DIR}/bench_forced_regression/stdout.json"
if [ -f "${out_file}" ]; then
    regression_flag="$(jq -r '.summary.benchmarks.help.baseline.regression' "${out_file}" 2>/dev/null || echo "false")"
    e2e_assert_eq "forced regression marks help benchmark" "true" "${regression_flag}"
else
    e2e_fail "forced regression output file missing"
fi

# ---------------------------------------------------------------------------
# Case 6: ATC perf gate treats absolute drift as advisory when overhead is within budget
# ---------------------------------------------------------------------------
e2e_case_banner "atc_perf_gate_absolute_drift_is_advisory"
ATC_GATE_PASS_REPORT="${WORKDIR}/atc_gate_absolute_drift_report.json"
ATC_GATE_PASS_OUT="${WORKDIR}/atc_gate_absolute_drift_out"
cat > "${ATC_GATE_PASS_REPORT}" <<'JSON'
{
  "summary": {
    "benchmarks": {
      "mail_send_no_atc": {
        "p95_ms": 250.0,
        "baseline_p95_ms": 205.78,
        "delta_p95_ms": 44.22,
        "regression": true
      },
      "mail_send_atc_shadow": {
        "p95_ms": 255.0,
        "baseline_p95_ms": 206.13,
        "delta_p95_ms": 48.87,
        "regression": true
      },
      "mail_send_atc_live": {
        "p95_ms": 260.0,
        "baseline_p95_ms": 214.05,
        "delta_p95_ms": 45.95,
        "regression": true
      }
    }
  },
  "failures": []
}
JSON

set +e
bash "${PROJECT_ROOT}/scripts/bench_atc_perf_gate.sh" \
    --skip-bench \
    --baseline "${PROJECT_ROOT}/benches/atc_perf_baseline.json" \
    --bench-report "${ATC_GATE_PASS_REPORT}" \
    --output-dir "${ATC_GATE_PASS_OUT}"
atc_gate_pass_rc=$?
set -e

e2e_assert_eq "ATC gate ignores absolute drift-only regressions" "0" "${atc_gate_pass_rc}"
e2e_assert_eq \
    "ATC gate summary stays pass for advisory drift" \
    "pass" \
    "$(jq -r '.status' "${ATC_GATE_PASS_OUT}/summary.json")"
e2e_assert_eq \
    "ATC gate preserves advisory baseline drift list" \
    "3" \
    "$(jq -r '.baseline_regressions | length' "${ATC_GATE_PASS_OUT}/summary.json")"

# ---------------------------------------------------------------------------
# Case 7: ATC perf gate fails when relative overhead breaches the budget
# ---------------------------------------------------------------------------
e2e_case_banner "atc_perf_gate_overhead_regression_exit_code"
ATC_GATE_FAIL_REPORT="${WORKDIR}/atc_gate_overhead_regression_report.json"
ATC_GATE_FAIL_OUT="${WORKDIR}/atc_gate_overhead_regression_out"
cat > "${ATC_GATE_FAIL_REPORT}" <<'JSON'
{
  "summary": {
    "benchmarks": {
      "mail_send_no_atc": {
        "p95_ms": 250.0,
        "baseline_p95_ms": 205.78,
        "delta_p95_ms": 44.22,
        "regression": false
      },
      "mail_send_atc_shadow": {
        "p95_ms": 255.0,
        "baseline_p95_ms": 206.13,
        "delta_p95_ms": 48.87,
        "regression": false
      },
      "mail_send_atc_live": {
        "p95_ms": 270.0,
        "baseline_p95_ms": 214.05,
        "delta_p95_ms": 55.95,
        "regression": false
      }
    }
  },
  "failures": []
}
JSON

set +e
bash "${PROJECT_ROOT}/scripts/bench_atc_perf_gate.sh" \
    --skip-bench \
    --baseline "${PROJECT_ROOT}/benches/atc_perf_baseline.json" \
    --bench-report "${ATC_GATE_FAIL_REPORT}" \
    --output-dir "${ATC_GATE_FAIL_OUT}"
atc_gate_fail_rc=$?
set -e

e2e_assert_eq "ATC gate returns exit code 3 on overhead regression" "3" "${atc_gate_fail_rc}"
e2e_assert_eq \
    "ATC gate summary flips to regression when overhead budget is breached" \
    "regression" \
    "$(jq -r '.status' "${ATC_GATE_FAIL_OUT}/summary.json")"

popd >/dev/null

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
e2e_summary
