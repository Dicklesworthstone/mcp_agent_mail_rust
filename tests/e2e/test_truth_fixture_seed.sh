#!/usr/bin/env bash
# test_truth_fixture_seed.sh - E2E regression suite for truth-incident fixture seeding.
#
# Verifies (br-2k3qx.1.1):
# - fixture builder seeds high-cardinality deterministic dataset
# - seeded dataset satisfies required minimum truth thresholds
# - markdown/state coverage fields are populated
# - identical seed/config yields identical dataset fingerprint
# - different seed yields a different fingerprint

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export E2E_SUITE="truth_fixture_seed"

# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Truth Fixture Seed E2E Suite"

assert_int_ge() {
    local label="$1"
    local value="$2"
    local min="$3"
    if [[ "${value}" =~ ^[0-9]+$ ]] && [ "${value}" -ge "${min}" ]; then
        e2e_pass "${label} (value=${value} >= ${min})"
    else
        e2e_fail "${label} (value=${value}, expected >= ${min})"
    fi
}

assert_not_eq() {
    local label="$1"
    local left="$2"
    local right="$3"
    if [ "${left}" != "${right}" ]; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
        e2e_diff "${label}" "value different from ${right}" "${left}"
    fi
}

for required_cmd in python3 jq sqlite3 curl; do
    if ! command -v "${required_cmd}" >/dev/null 2>&1; then
        e2e_log "${required_cmd} not found; skipping suite"
        e2e_skip "${required_cmd} required"
        e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"
        e2e_summary
        exit 0
    fi
done

if ! command -v mcp-agent-mail >/dev/null 2>&1; then
    if [ -x "${CARGO_TARGET_DIR}/debug/mcp-agent-mail" ]; then
        export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
    fi
fi
if ! command -v mcp-agent-mail >/dev/null 2>&1; then
    e2e_log "mcp-agent-mail binary not found; skipping suite"
    e2e_skip "mcp-agent-mail required for schema bootstrap"
    e2e_summary
    exit 0
fi

FIXTURE_SCRIPT="${SCRIPT_DIR}/../../scripts/seed_truth_incident_fixture.sh"
e2e_case_banner "fixture script presence"
e2e_mark_case_start "case01_fixture_script_presence"
e2e_assert_file_exists "fixture script exists" "${FIXTURE_SCRIPT}"
if [ -x "${FIXTURE_SCRIPT}" ]; then
    e2e_pass "fixture script is executable"
else
    e2e_fail "fixture script is executable"
fi

WORK="$(e2e_mktemp "e2e_truth_fixture_seed")"
DB_PATH="${WORK}/truth_fixture.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
mkdir -p "${STORAGE_ROOT}"

PROJECTS=15
AGENTS=300
MESSAGES=3200
THREADS=320
SEED_A=20260302
SEED_B=20260303

REPORT_A="${WORK}/report_seed_a.json"
REPORT_A_RERUN="${WORK}/report_seed_a_rerun.json"
REPORT_B="${WORK}/report_seed_b.json"
STDOUT_A="${WORK}/run_seed_a.stdout"
STDERR_A="${WORK}/run_seed_a.stderr"
STDOUT_A_RERUN="${WORK}/run_seed_a_rerun.stdout"
STDERR_A_RERUN="${WORK}/run_seed_a_rerun.stderr"
STDOUT_B="${WORK}/run_seed_b.stdout"
STDERR_B="${WORK}/run_seed_b.stderr"

run_fixture_seed() {
    local seed="$1"
    local report_path="$2"
    local stdout_path="$3"
    local stderr_path="$4"

    set +e
    "${FIXTURE_SCRIPT}" \
        --db "${DB_PATH}" \
        --storage-root "${STORAGE_ROOT}" \
        --report "${report_path}" \
        --projects "${PROJECTS}" \
        --agents "${AGENTS}" \
        --messages "${MESSAGES}" \
        --threads "${THREADS}" \
        --seed "${seed}" \
        --overwrite \
        >"${stdout_path}" 2>"${stderr_path}"
    local rc=$?
    set -e

    echo "${rc}"
}

e2e_case_banner "seed fixture: initial run with canonical seed"
e2e_mark_case_start "case02_seed_fixture_initial_run_with_canonical_seed"
RC_A="$(run_fixture_seed "${SEED_A}" "${REPORT_A}" "${STDOUT_A}" "${STDERR_A}")"
e2e_save_artifact "seed_a_stdout.json" "$(cat "${STDOUT_A}" 2>/dev/null || true)"
e2e_save_artifact "seed_a_stderr.log" "$(cat "${STDERR_A}" 2>/dev/null || true)"
e2e_assert_exit_code "fixture seed run (seed A)" "0" "${RC_A}"
e2e_assert_file_exists "seed A report exists" "${REPORT_A}"
e2e_assert_file_exists "seed A database exists" "${DB_PATH}"

if jq -e '.ok == true' "${REPORT_A}" >/dev/null 2>&1; then
    e2e_pass "seed A report marked ok"
else
    e2e_fail "seed A report marked ok"
fi

COUNT_PROJECTS="$(jq -r '.counts.projects' "${REPORT_A}")"
COUNT_AGENTS="$(jq -r '.counts.agents' "${REPORT_A}")"
COUNT_MESSAGES="$(jq -r '.counts.messages' "${REPORT_A}")"
COUNT_THREADS="$(jq -r '.counts.threads' "${REPORT_A}")"
COUNT_ACK_REQUIRED="$(jq -r '.state_mix.ack_required_messages' "${REPORT_A}")"
COUNT_RECIPIENT_READS="$(jq -r '.state_mix.recipient_reads' "${REPORT_A}")"
COUNT_RECIPIENT_ACKS="$(jq -r '.state_mix.recipient_acks' "${REPORT_A}")"
COUNT_HEADINGS="$(jq -r '.markdown_coverage.heading_rows' "${REPORT_A}")"
COUNT_CODE="$(jq -r '.markdown_coverage.code_rows' "${REPORT_A}")"
COUNT_TABLES="$(jq -r '.markdown_coverage.table_rows' "${REPORT_A}")"
COUNT_LINKS="$(jq -r '.markdown_coverage.link_rows' "${REPORT_A}")"
COUNT_QUOTES="$(jq -r '.markdown_coverage.quote_rows' "${REPORT_A}")"
COUNT_TASKS="$(jq -r '.markdown_coverage.task_rows' "${REPORT_A}")"
MAX_BODY_LEN="$(jq -r '.markdown_coverage.max_body_len' "${REPORT_A}")"
FINGERPRINT_A="$(jq -r '.dataset_fingerprint_sha256' "${REPORT_A}")"

e2e_assert_eq "projects count matches request" "${PROJECTS}" "${COUNT_PROJECTS}"
e2e_assert_eq "agents count matches request" "${AGENTS}" "${COUNT_AGENTS}"
e2e_assert_eq "messages count matches request" "${MESSAGES}" "${COUNT_MESSAGES}"
e2e_assert_eq "threads count matches request" "${THREADS}" "${COUNT_THREADS}"
assert_int_ge "ack_required message count is non-zero" "${COUNT_ACK_REQUIRED}" 1
assert_int_ge "recipient read count is non-zero" "${COUNT_RECIPIENT_READS}" 1
assert_int_ge "recipient ack count is non-zero" "${COUNT_RECIPIENT_ACKS}" 1
assert_int_ge "markdown heading coverage is non-zero" "${COUNT_HEADINGS}" 1
assert_int_ge "markdown code coverage is non-zero" "${COUNT_CODE}" 1
assert_int_ge "markdown table coverage is non-zero" "${COUNT_TABLES}" 1
assert_int_ge "markdown link coverage is non-zero" "${COUNT_LINKS}" 1
assert_int_ge "markdown quote coverage is non-zero" "${COUNT_QUOTES}" 1
assert_int_ge "markdown task coverage is non-zero" "${COUNT_TASKS}" 1
assert_int_ge "max body length reflects multiline content" "${MAX_BODY_LEN}" 80

DB_PROJECTS="$(sqlite3 "${DB_PATH}" 'SELECT COUNT(*) FROM projects;')"
DB_AGENTS="$(sqlite3 "${DB_PATH}" 'SELECT COUNT(*) FROM agents;')"
DB_MESSAGES="$(sqlite3 "${DB_PATH}" 'SELECT COUNT(*) FROM messages;')"
DB_THREADS="$(sqlite3 "${DB_PATH}" "SELECT COUNT(DISTINCT thread_id) FROM messages WHERE thread_id IS NOT NULL AND thread_id != '';")"
DB_ACK_REQUIRED="$(sqlite3 "${DB_PATH}" 'SELECT COUNT(*) FROM messages WHERE ack_required = 1;')"
e2e_assert_eq "sqlite projects matches report" "${COUNT_PROJECTS}" "${DB_PROJECTS}"
e2e_assert_eq "sqlite agents matches report" "${COUNT_AGENTS}" "${DB_AGENTS}"
e2e_assert_eq "sqlite messages matches report" "${COUNT_MESSAGES}" "${DB_MESSAGES}"
e2e_assert_eq "sqlite threads matches report" "${COUNT_THREADS}" "${DB_THREADS}"
e2e_assert_eq "sqlite ack_required matches report" "${COUNT_ACK_REQUIRED}" "${DB_ACK_REQUIRED}"

e2e_case_banner "seed fixture: deterministic rerun with same seed"
e2e_mark_case_start "case03_seed_fixture_deterministic_rerun_with_same_seed"
RC_A_RERUN="$(run_fixture_seed "${SEED_A}" "${REPORT_A_RERUN}" "${STDOUT_A_RERUN}" "${STDERR_A_RERUN}")"
e2e_save_artifact "seed_a_rerun_stdout.json" "$(cat "${STDOUT_A_RERUN}" 2>/dev/null || true)"
e2e_save_artifact "seed_a_rerun_stderr.log" "$(cat "${STDERR_A_RERUN}" 2>/dev/null || true)"
e2e_assert_exit_code "fixture rerun (seed A)" "0" "${RC_A_RERUN}"
e2e_assert_file_exists "seed A rerun report exists" "${REPORT_A_RERUN}"
FINGERPRINT_A_RERUN="$(jq -r '.dataset_fingerprint_sha256' "${REPORT_A_RERUN}")"
COUNTS_A_JSON="$(jq -c '.counts' "${REPORT_A}")"
COUNTS_A_RERUN_JSON="$(jq -c '.counts' "${REPORT_A_RERUN}")"
STATE_A_JSON="$(jq -c '.state_mix' "${REPORT_A}")"
STATE_A_RERUN_JSON="$(jq -c '.state_mix' "${REPORT_A_RERUN}")"
MARKDOWN_A_JSON="$(jq -c '.markdown_coverage' "${REPORT_A}")"
MARKDOWN_A_RERUN_JSON="$(jq -c '.markdown_coverage' "${REPORT_A_RERUN}")"
e2e_assert_eq "same seed fingerprint is deterministic" "${FINGERPRINT_A}" "${FINGERPRINT_A_RERUN}"
e2e_assert_eq "same seed counts are deterministic" "${COUNTS_A_JSON}" "${COUNTS_A_RERUN_JSON}"
e2e_assert_eq "same seed state mix is deterministic" "${STATE_A_JSON}" "${STATE_A_RERUN_JSON}"
e2e_assert_eq "same seed markdown coverage is deterministic" "${MARKDOWN_A_JSON}" "${MARKDOWN_A_RERUN_JSON}"

e2e_case_banner "seed fixture: different seed changes fingerprint"
e2e_mark_case_start "case04_seed_fixture_different_seed_changes_fingerprint"
RC_B="$(run_fixture_seed "${SEED_B}" "${REPORT_B}" "${STDOUT_B}" "${STDERR_B}")"
e2e_save_artifact "seed_b_stdout.json" "$(cat "${STDOUT_B}" 2>/dev/null || true)"
e2e_save_artifact "seed_b_stderr.log" "$(cat "${STDERR_B}" 2>/dev/null || true)"
e2e_assert_exit_code "fixture seed run (seed B)" "0" "${RC_B}"
e2e_assert_file_exists "seed B report exists" "${REPORT_B}"
FINGERPRINT_B="$(jq -r '.dataset_fingerprint_sha256' "${REPORT_B}")"
assert_not_eq "different seed changes dataset fingerprint" "${FINGERPRINT_A}" "${FINGERPRINT_B}"

e2e_save_artifact "seed_a_report.json" "$(cat "${REPORT_A}")"
e2e_save_artifact "seed_a_rerun_report.json" "$(cat "${REPORT_A_RERUN}")"
e2e_save_artifact "seed_b_report.json" "$(cat "${REPORT_B}")"
e2e_save_db_snapshot "${DB_PATH}" "projects" "projects_snapshot.txt"
e2e_save_db_snapshot "${DB_PATH}" "agents" "agents_snapshot.txt"
e2e_save_db_snapshot "${DB_PATH}" "messages" "messages_snapshot.txt"
e2e_save_db_snapshot "${DB_PATH}" "message_recipients" "message_recipients_snapshot.txt"

if ! e2e_summary; then
    exit 1
fi
exit 0
