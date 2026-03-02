#!/usr/bin/env bash
# test_incident_capture_harness.sh - E2E contract for incident capture harness.
#
# Verifies (br-2k3qx.1.3):
# - one-command seed/run/capture generates complete artifact bundle
# - key snapshots exist for dashboard/messages/threads/agents/projects/health
# - outputs include DB truth diagnostics and manifest metadata
# - same seed yields stable fixture fingerprint across runs

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export E2E_SUITE="incident_capture_harness"

# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Incident Capture Harness E2E Suite"

assert_not_empty_file() {
    local label="$1"
    local file_path="$2"
    if [ -s "${file_path}" ]; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
    fi
}

for required_cmd in python3 jq sqlite3 curl tar; do
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

HARNESS_SCRIPT="${SCRIPT_DIR}/../../scripts/incident_capture_harness.sh"
e2e_case_banner "harness script presence"
e2e_mark_case_start "case01_harness_script_presence"
e2e_assert_file_exists "incident capture harness exists" "${HARNESS_SCRIPT}"
if [ -x "${HARNESS_SCRIPT}" ]; then
    e2e_pass "incident capture harness is executable"
else
    e2e_fail "incident capture harness is executable"
fi

WORK="$(e2e_mktemp "e2e_incident_capture")"
OUT_A="${WORK}/capture_a"
OUT_B="${WORK}/capture_b"
SEED=20260302

run_harness() {
    local output_dir="$1"
    local stdout_file="$2"
    local stderr_file="$3"
    set +e
    "${HARNESS_SCRIPT}" \
        --output-dir "${output_dir}" \
        --seed "${SEED}" \
        --robot-timeout-secs 3 \
        --verbose \
        >"${stdout_file}" 2>"${stderr_file}"
    local rc=$?
    set -e
    echo "${rc}"
}

e2e_case_banner "incident capture run A produces complete artifact set"
e2e_mark_case_start "case02_incident_capture_run_a_produces_complete_artifact_"
RC_A="$(run_harness "${OUT_A}" "${WORK}/run_a.stdout" "${WORK}/run_a.stderr")"
e2e_save_artifact "run_a_stdout.json" "$(cat "${WORK}/run_a.stdout" 2>/dev/null || true)"
e2e_save_artifact "run_a_stderr.log" "$(cat "${WORK}/run_a.stderr" 2>/dev/null || true)"
e2e_assert_exit_code "harness run A exits cleanly" "0" "${RC_A}"

e2e_assert_file_exists "manifest exists" "${OUT_A}/incident_capture_manifest.json"
e2e_assert_file_exists "bundle exists" "${OUT_A}/incident_capture_bundle.tar.gz"
e2e_assert_file_exists "fixture report exists" "${OUT_A}/fixture_report.json"
e2e_assert_file_exists "context exists" "${OUT_A}/context.json"
e2e_assert_file_exists "db counts exists" "${OUT_A}/diagnostics/db_counts.tsv"
e2e_assert_file_exists "server log exists" "${OUT_A}/logs/server.log"
e2e_assert_file_exists "dashboard snapshot exists" "${OUT_A}/snapshots/dashboard_status.json"
e2e_assert_file_exists "messages snapshot exists" "${OUT_A}/snapshots/messages_inbox.json"
e2e_assert_file_exists "messages limit snapshot exists" "${OUT_A}/snapshots/messages_inbox_limit5.json"
e2e_assert_file_exists "messages unread snapshot exists" "${OUT_A}/snapshots/messages_inbox_unread.json"
e2e_assert_file_exists "message detail snapshot exists" "${OUT_A}/snapshots/message_detail.json"
e2e_assert_file_exists "thread snapshot exists" "${OUT_A}/snapshots/threads_view.md"
e2e_assert_file_exists "thread limit snapshot exists" "${OUT_A}/snapshots/threads_view_limit3.json"
e2e_assert_file_exists "agents snapshot exists" "${OUT_A}/snapshots/agents_view.json"
e2e_assert_file_exists "projects snapshot exists" "${OUT_A}/snapshots/projects_view.json"
e2e_assert_file_exists "system health snapshot exists" "${OUT_A}/snapshots/system_health.json"
e2e_assert_file_exists "http health snapshot exists" "${OUT_A}/snapshots/http_health.json"
e2e_assert_file_exists "mail root html exists" "${OUT_A}/snapshots/mail_root.html"
e2e_assert_file_exists "mail root headers exists" "${OUT_A}/snapshots/mail_root_headers.txt"

assert_not_empty_file "threads markdown snapshot is non-empty" "${OUT_A}/snapshots/threads_view.md"
assert_not_empty_file "mail root html is non-empty" "${OUT_A}/snapshots/mail_root.html"
assert_not_empty_file "db counts report is non-empty" "${OUT_A}/diagnostics/db_counts.tsv"

if jq -e '.ok == true' "${OUT_A}/fixture_report.json" >/dev/null 2>&1; then
    e2e_pass "fixture report A marked ok"
else
    e2e_fail "fixture report A marked ok"
fi
if jq -e '.counts.projects >= 15 and .counts.agents >= 300 and .counts.messages >= 3000 and .counts.threads >= 300' "${OUT_A}/fixture_report.json" >/dev/null 2>&1; then
    e2e_pass "fixture report A meets incident thresholds"
else
    e2e_fail "fixture report A meets incident thresholds"
fi
if jq -e '.seed == 20260302' "${OUT_A}/context.json" >/dev/null 2>&1; then
    e2e_pass "context seed matches requested deterministic seed"
else
    e2e_fail "context seed matches requested deterministic seed"
fi
if jq -e '.project_key != "" and .agent_name != "" and .thread_id != "" and .message_id > 0' "${OUT_A}/context.json" >/dev/null 2>&1; then
    e2e_pass "context includes resolved project/agent/thread/message anchors"
else
    e2e_fail "context includes resolved project/agent/thread/message anchors"
fi

for json_snap in \
    "${OUT_A}/snapshots/dashboard_status.json" \
    "${OUT_A}/snapshots/messages_inbox.json" \
    "${OUT_A}/snapshots/messages_inbox_limit5.json" \
    "${OUT_A}/snapshots/messages_inbox_unread.json" \
    "${OUT_A}/snapshots/message_detail.json" \
    "${OUT_A}/snapshots/threads_view_limit3.json" \
    "${OUT_A}/snapshots/agents_view.json" \
    "${OUT_A}/snapshots/projects_view.json" \
    "${OUT_A}/snapshots/system_health.json" \
    "${OUT_A}/snapshots/http_health.json" \
    "${OUT_A}/snapshots/timeline.json" \
    "${OUT_A}/snapshots/overview.json" \
    "${OUT_A}/snapshots/search_results.json" \
    "${OUT_A}/snapshots/search_results_since.json" \
    "${OUT_A}/snapshots/search_results_kind_message.json" \
    "${OUT_A}/snapshots/reservations.json" \
    "${OUT_A}/snapshots/metrics.json" \
    "${OUT_A}/snapshots/analytics.json" \
    "${OUT_A}/snapshots/contacts.json" \
    "${OUT_A}/snapshots/attachments.json"
do
    if jq . "${json_snap}" >/dev/null 2>&1; then
        e2e_pass "valid JSON snapshot: $(basename "${json_snap}")"
    else
        e2e_fail "valid JSON snapshot: $(basename "${json_snap}")"
    fi
done

DB_COUNTS_CONTENT="$(cat "${OUT_A}/diagnostics/db_counts.tsv")"
for required_row in projects agents messages threads ack_required_messages recipient_rows recipient_reads recipient_acks; do
    if echo "${DB_COUNTS_CONTENT}" | rg -q "^${required_row}[[:space:]]"; then
        e2e_pass "db counts includes ${required_row}"
    else
        e2e_fail "db counts includes ${required_row}"
    fi
done

# Phase 3b-4 artifacts: structured DB truth + truth comparison
e2e_assert_file_exists "db truth json exists" "${OUT_A}/diagnostics/db_truth.json"
e2e_assert_file_exists "truth comparison exists" "${OUT_A}/diagnostics/truth_comparison.json"
assert_not_empty_file "db truth json is non-empty" "${OUT_A}/diagnostics/db_truth.json"
assert_not_empty_file "truth comparison is non-empty" "${OUT_A}/diagnostics/truth_comparison.json"

if jq -e '.global_counts.projects >= 15 and .global_counts.agents >= 300' "${OUT_A}/diagnostics/db_truth.json" >/dev/null 2>&1; then
    e2e_pass "db truth global counts meet fixture thresholds"
else
    e2e_fail "db truth global counts meet fixture thresholds"
fi
if jq -e '.per_project | keys | length >= 15' "${OUT_A}/diagnostics/db_truth.json" >/dev/null 2>&1; then
    e2e_pass "db truth per_project has all projects"
else
    e2e_fail "db truth per_project has all projects"
fi
if jq -e '.verdict' "${OUT_A}/diagnostics/truth_comparison.json" >/dev/null 2>&1; then
    e2e_pass "truth comparison has verdict field"
else
    e2e_fail "truth comparison has verdict field"
fi
if jq -e '.schema_version == "truth_oracle_report.v1" and (.summary.check_count == (.checks | length))' "${OUT_A}/diagnostics/truth_comparison.json" >/dev/null 2>&1; then
    e2e_pass "truth comparison includes normalized checks summary"
else
    e2e_fail "truth comparison includes normalized checks summary"
fi
if jq -e '
    def has_check($id): any(.checks[]?; .check_id == $id);
    has_check("robot.projects:projects.count")
    and has_check("robot.messages:messages.inbox_total_lte_global")
    and has_check("robot.status:top_threads.gte_one_if_any_threads")
    and has_check("robot.thread:message_count.gte_one_if_any_threads")
    and has_check("robot.search:total_results.gte_one_if_any_messages")
    and has_check("robot.archive:attachments.inventory_nonnegative")
' "${OUT_A}/diagnostics/truth_comparison.json" >/dev/null 2>&1; then
    e2e_pass "truth comparison covers H2 robot surface families"
else
    e2e_fail "truth comparison covers H2 robot surface families"
fi
if jq -e '
    def has_check($id): any(.checks[]?; .check_id == $id);
    has_check("robot.messages_params:inbox_limit5.lte_baseline")
    and has_check("robot.messages_params:inbox_limit5.lte_5")
    and has_check("robot.messages_params:inbox_unread.lte_baseline")
    and has_check("robot.thread_params:thread_limit3.lte_baseline")
    and has_check("robot.thread_params:thread_limit3.lte_3")
    and has_check("robot.search_params:search_since.lte_baseline")
    and has_check("robot.search_params:search_kind_message.lte_baseline")
' "${OUT_A}/diagnostics/truth_comparison.json" >/dev/null 2>&1; then
    e2e_pass "truth comparison covers H3 query param-sweep invariants"
else
    e2e_fail "truth comparison covers H3 query param-sweep invariants"
fi

# Phase 5 artifacts: enhanced context + repro script
e2e_assert_file_exists "repro script exists" "${OUT_A}/repro.sh"
if [ -x "${OUT_A}/repro.sh" ]; then
    e2e_pass "repro script is executable"
else
    e2e_fail "repro script is executable"
fi
if jq -e '.bead_id == "br-2k3qx.1.3"' "${OUT_A}/context.json" >/dev/null 2>&1; then
    e2e_pass "context includes bead_id"
else
    e2e_fail "context includes bead_id"
fi
if jq -e '.capture_stats.succeeded >= 0' "${OUT_A}/context.json" >/dev/null 2>&1; then
    e2e_pass "context includes capture_stats"
else
    e2e_fail "context includes capture_stats"
fi
if jq -e '.fixture_params.projects == 15' "${OUT_A}/context.json" >/dev/null 2>&1; then
    e2e_pass "context includes fixture_params"
else
    e2e_fail "context includes fixture_params"
fi

# Phase 7: enhanced manifest with verdict
if jq -e '.summary.verdict' "${OUT_A}/incident_capture_manifest.json" >/dev/null 2>&1; then
    e2e_pass "manifest includes summary verdict"
else
    e2e_fail "manifest includes summary verdict"
fi
if jq -e '.snapshots | keys | length >= 10' "${OUT_A}/incident_capture_manifest.json" >/dev/null 2>&1; then
    e2e_pass "manifest enumerates snapshots"
else
    e2e_fail "manifest enumerates snapshots"
fi

BUNDLE_LIST="$(tar -tzf "${OUT_A}/incident_capture_bundle.tar.gz")"
e2e_save_artifact "bundle_a_contents.txt" "${BUNDLE_LIST}"
for required_path in \
    fixture_report.json \
    context.json \
    repro.sh \
    diagnostics/db_counts.tsv \
    diagnostics/db_truth.json \
    diagnostics/truth_comparison.json \
    logs/server.log \
    snapshots/dashboard_status.json \
    snapshots/messages_inbox.json \
    snapshots/threads_view.md \
    snapshots/agents_view.json \
    snapshots/projects_view.json \
    snapshots/system_health.json
do
    if echo "${BUNDLE_LIST}" | rg -q "^${required_path}$"; then
        e2e_pass "bundle includes ${required_path}"
    else
        e2e_fail "bundle includes ${required_path}"
    fi
done

e2e_case_banner "incident capture run B preserves deterministic fingerprint"
e2e_mark_case_start "case03_incident_capture_run_b_preserves_deterministic_fin"
RC_B="$(run_harness "${OUT_B}" "${WORK}/run_b.stdout" "${WORK}/run_b.stderr")"
e2e_save_artifact "run_b_stdout.json" "$(cat "${WORK}/run_b.stdout" 2>/dev/null || true)"
e2e_save_artifact "run_b_stderr.log" "$(cat "${WORK}/run_b.stderr" 2>/dev/null || true)"
e2e_assert_exit_code "harness run B exits cleanly" "0" "${RC_B}"

FP_A="$(jq -r '.dataset_fingerprint_sha256' "${OUT_A}/fixture_report.json")"
FP_B="$(jq -r '.dataset_fingerprint_sha256' "${OUT_B}/fixture_report.json")"
e2e_assert_eq "same seed fixture fingerprint remains stable" "${FP_A}" "${FP_B}"

CONTEXT_FP_A="$(jq -r '.fixture_fingerprint_sha256' "${OUT_A}/context.json")"
CONTEXT_FP_B="$(jq -r '.fixture_fingerprint_sha256' "${OUT_B}/context.json")"
e2e_assert_eq "context fingerprints match report fingerprints (run A)" "${FP_A}" "${CONTEXT_FP_A}"
e2e_assert_eq "context fingerprints match report fingerprints (run B)" "${FP_B}" "${CONTEXT_FP_B}"

if ! e2e_summary; then
    exit 1
fi
exit 0
