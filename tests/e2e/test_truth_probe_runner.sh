#!/usr/bin/env bash
# test_truth_probe_runner.sh - E2E contract suite for G2 truth probe runner.
#
# Verifies:
# - runner emits machine-readable report with mismatch severity + repro metadata
# - deterministic mode works
# - high-cardinality mode works
# - contract handles mismatch exit code (2) distinctly from fatal errors

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export E2E_SUITE="truth_probe_runner"

# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Truth Probe Runner E2E Suite"

assert_exit_ok_or_mismatch() {
    local label="$1"
    local rc="$2"
    if [ "${rc}" -eq 0 ] || [ "${rc}" -eq 2 ]; then
        e2e_pass "${label} (exit=${rc})"
    else
        e2e_fail "${label} (exit=${rc})"
    fi
}

assert_h2_surface_family_coverage() {
    local label="$1"
    local report_path="$2"

    if jq -e '.schema_version == "truth_oracle_report.v1" and (.summary.check_count == (.checks | length))' "${report_path}" >/dev/null 2>&1; then
        e2e_pass "${label}: normalized oracle schema present"
    else
        e2e_fail "${label}: normalized oracle schema present"
    fi

    if jq -e '
        def has_surface($id):
            any(.surface_results[]; .surface_id == $id and .status == "probed" and (.checks | length) > 0);
        has_surface("tui.dashboard")
        and has_surface("tui.messages")
        and has_surface("tui.threads")
        and has_surface("tui.agents")
        and has_surface("tui.projects")
        and has_surface("tui.system_health")
        and has_surface("tui.search")
        and has_surface("tui.archive_browser")
    ' "${report_path}" >/dev/null 2>&1; then
        e2e_pass "${label}: major tui surface families are probed"
    else
        e2e_fail "${label}: major tui surface families are probed"
    fi

    if jq -e '
        def cmd_probed($name):
            any(.robot_family_results[]?.commands[]?; .command == $name and .status == "probed" and (.checks | length) > 0);
        (.robot_family_results | length) >= 4
        and cmd_probed("status")
        and cmd_probed("inbox")
        and cmd_probed("overview")
        and cmd_probed("thread")
        and cmd_probed("search")
        and cmd_probed("navigate")
        and cmd_probed("attachments")
    ' "${report_path}" >/dev/null 2>&1; then
        e2e_pass "${label}: robot command families are integrated"
    else
        e2e_fail "${label}: robot command families are integrated"
    fi
}

assert_h3_parameter_sweeps() {
    local label="$1"
    local report_path="$2"

    if jq -e '
        def has_check($id):
            any(.checks[]?; .check_id == $id);
        has_check("robot.inbox_params:limit5.lte_baseline")
        and has_check("robot.inbox_params:limit5.lte_5")
        and has_check("robot.inbox_params:unread.lte_baseline")
        and has_check("robot.thread_params:limit3.lte_baseline")
        and has_check("robot.thread_params:limit3.lte_3")
        and has_check("robot.search_params:since.lte_baseline")
        and has_check("robot.search_params:kind_message.lte_baseline")
    ' "${report_path}" >/dev/null 2>&1; then
        e2e_pass "${label}: H3 param-sweep checks are present"
    else
        e2e_fail "${label}: H3 param-sweep checks are present"
    fi
}

assert_h4_stress_contract() {
    local label="$1"
    local report_path="$2"

    if jq -e '
        (.stress_mode_results | length) >= 1
        and (.stress_mode_results[0].runtime_budget_secs >= 180)
        and (.stress_mode_results[0].elapsed_secs >= 0)
        and (.culprit_surface_map != null)
    ' "${report_path}" >/dev/null 2>&1; then
        e2e_pass "${label}: stress metadata and culprit map present"
    else
        e2e_fail "${label}: stress metadata and culprit map present"
    fi
}

assert_h4_high_cardinality_checks() {
    local label="$1"
    local report_path="$2"

    if jq -e '
        def has_check($id):
            any(.checks[]?; .check_id == $id);
        has_check("stress.high_cardinality:harness.elapsed_secs.lte_budget")
        and has_check("stress.high_cardinality:capture.failures.eq_zero")
        and has_check("stress.high_cardinality:truth_comparison.present")
        and has_check("stress.high_cardinality:db.projects.eq_fixture_param")
        and has_check("stress.high_cardinality:db.agents.eq_fixture_param")
        and has_check("stress.high_cardinality:db.messages.eq_fixture_param")
        and has_check("stress.high_cardinality:db.threads.eq_fixture_param")
        and has_check("stress.high_cardinality:db.per_project_count.gte_fixture_projects")
    ' "${report_path}" >/dev/null 2>&1; then
        e2e_pass "${label}: high-cardinality stress checks are present"
    else
        e2e_fail "${label}: high-cardinality stress checks are present"
    fi
}

for required_cmd in python3 jq sqlite3 curl; do
    if ! command -v "${required_cmd}" >/dev/null 2>&1; then
        e2e_log "${required_cmd} missing; skipping suite"
        e2e_skip "${required_cmd} required"
        e2e_summary
        exit 0
    fi
done

if ! command -v mcp-agent-mail >/dev/null 2>&1 || ! command -v am >/dev/null 2>&1; then
    if [ -x "${CARGO_TARGET_DIR}/debug/mcp-agent-mail" ] && [ -x "${CARGO_TARGET_DIR}/debug/am" ]; then
        export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
    fi
fi
if ! command -v mcp-agent-mail >/dev/null 2>&1 || ! command -v am >/dev/null 2>&1; then
    e2e_log "mcp-agent-mail/am binaries unavailable; skipping suite"
    e2e_skip "mcp-agent-mail + am required"
    e2e_summary
    exit 0
fi

RUNNER_SCRIPT="${SCRIPT_DIR}/../../scripts/truth_probe_runner.sh"
CATALOG_PATH="${SCRIPT_DIR}/../../docs/INCIDENT_BR_2K3QX_G1_SURFACE_QUERY_CATALOG.json"

e2e_case_banner "runner script presence"
e2e_mark_case_start "case01_runner_script_presence"
e2e_assert_file_exists "truth_probe_runner.sh exists" "${RUNNER_SCRIPT}"
e2e_assert_file_exists "G1 catalog exists" "${CATALOG_PATH}"
if [ -x "${RUNNER_SCRIPT}" ]; then
    e2e_pass "truth_probe_runner.sh is executable"
else
    e2e_fail "truth_probe_runner.sh is executable"
fi

WORK="$(e2e_mktemp "e2e_truth_probe_runner")"
OUT_DET="${WORK}/deterministic_run"
OUT_HIGH="${WORK}/high_card_run"

run_probe() {
    local output_dir="$1"
    shift
    local stdout_path="${output_dir}.stdout.json"
    local stderr_path="${output_dir}.stderr.log"
    mkdir -p "${output_dir}"
    set +e
    "${RUNNER_SCRIPT}" "$@" \
        --catalog "${CATALOG_PATH}" \
        --output-dir "${output_dir}" \
        --robot-timeout-secs 3 \
        >"${stdout_path}" 2>"${stderr_path}"
    local rc=$?
    set -e
    e2e_save_artifact "$(basename "${output_dir}")_stdout.json" "$(cat "${stdout_path}" 2>/dev/null || true)"
    e2e_save_artifact "$(basename "${output_dir}")_stderr.log" "$(cat "${stderr_path}" 2>/dev/null || true)"
    echo "${rc}"
}

e2e_case_banner "deterministic mode contract"
e2e_mark_case_start "case02_deterministic_mode_contract"
RC_DET="$(run_probe "${OUT_DET}" \
    --mode deterministic \
    --det-seed 20260302 \
    --det-projects 15 \
    --det-agents 300 \
    --det-messages 3000 \
    --det-threads 300 \
)"
assert_exit_ok_or_mismatch "deterministic mode exits with contract code" "${RC_DET}"
e2e_assert_file_exists "deterministic report exists" "${OUT_DET}/truth_probe_report.json"
if jq . "${OUT_DET}/truth_probe_report.json" >/dev/null 2>&1; then
    e2e_pass "deterministic report is valid JSON"
else
    e2e_fail "deterministic report is valid JSON"
fi
if jq -e '.mode_runs | length == 1 and .[0].mode == "deterministic"' "${OUT_DET}/truth_probe_report.json" >/dev/null 2>&1; then
    e2e_pass "deterministic report mode metadata correct"
else
    e2e_fail "deterministic report mode metadata correct"
fi
if jq -e '.surface_results | length > 0' "${OUT_DET}/truth_probe_report.json" >/dev/null 2>&1; then
    e2e_pass "deterministic report includes surface results"
else
    e2e_fail "deterministic report includes surface results"
fi
if jq -e '.mismatches | all((.severity != null) and (.repro.repro_script != null) and (.mode != null))' "${OUT_DET}/truth_probe_report.json" >/dev/null 2>&1; then
    e2e_pass "deterministic mismatches include severity + repro metadata"
else
    e2e_fail "deterministic mismatches include severity + repro metadata"
fi
assert_h2_surface_family_coverage "deterministic report" "${OUT_DET}/truth_probe_report.json"
assert_h3_parameter_sweeps "deterministic report" "${OUT_DET}/truth_probe_report.json"
assert_h4_stress_contract "deterministic report" "${OUT_DET}/truth_probe_report.json"

e2e_case_banner "high-cardinality mode contract"
e2e_mark_case_start "case03_highcardinality_mode_contract"
RC_HIGH="$(run_probe "${OUT_HIGH}" \
    --mode high-cardinality \
    --high-seed 20260303 \
    --high-projects 16 \
    --high-agents 320 \
    --high-messages 3200 \
    --high-threads 320 \
)"
assert_exit_ok_or_mismatch "high-cardinality mode exits with contract code" "${RC_HIGH}"
e2e_assert_file_exists "high-card report exists" "${OUT_HIGH}/truth_probe_report.json"
if jq . "${OUT_HIGH}/truth_probe_report.json" >/dev/null 2>&1; then
    e2e_pass "high-card report is valid JSON"
else
    e2e_fail "high-card report is valid JSON"
fi
if jq -e '.mode_runs | length == 1 and .[0].mode == "high_cardinality"' "${OUT_HIGH}/truth_probe_report.json" >/dev/null 2>&1; then
    e2e_pass "high-card report mode metadata correct"
else
    e2e_fail "high-card report mode metadata correct"
fi
if jq -e '.surface_results | length > 0' "${OUT_HIGH}/truth_probe_report.json" >/dev/null 2>&1; then
    e2e_pass "high-card report includes surface results"
else
    e2e_fail "high-card report includes surface results"
fi
if jq -e '.mismatches | all((.severity != null) and (.repro.repro_script != null) and (.mode != null))' "${OUT_HIGH}/truth_probe_report.json" >/dev/null 2>&1; then
    e2e_pass "high-card mismatches include severity + repro metadata"
else
    e2e_fail "high-card mismatches include severity + repro metadata"
fi
assert_h2_surface_family_coverage "high-card report" "${OUT_HIGH}/truth_probe_report.json"
assert_h3_parameter_sweeps "high-card report" "${OUT_HIGH}/truth_probe_report.json"
assert_h4_stress_contract "high-card report" "${OUT_HIGH}/truth_probe_report.json"
assert_h4_high_cardinality_checks "high-card report" "${OUT_HIGH}/truth_probe_report.json"

if ! e2e_summary; then
    exit 1
fi
exit 0
