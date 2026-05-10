#!/usr/bin/env bash
# Self-test for tests/e2e/lib helpers.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=structured_logging.sh
source "${SCRIPT_DIR}/structured_logging.sh"
# shellcheck source=spawn_server.sh
source "${SCRIPT_DIR}/spawn_server.sh"
# shellcheck source=seed_corpus.sh
source "${SCRIPT_DIR}/seed_corpus.sh"
# shellcheck source=wait_for.sh
source "${SCRIPT_DIR}/wait_for.sh"

PASS=0
FAIL=0
TOTAL=0
TMP_ROOT="${TMPDIR:-/data/tmp}"
if [ ! -d "${TMP_ROOT}" ]; then
    TMP_ROOT="/tmp"
fi
WORK="${TMP_ROOT%/}/am-e2e-lib-self-test.$(date -u +%Y%m%dT%H%M%SZ).$$"
mkdir -p "${WORK}"

record_pass() {
    TOTAL=$((TOTAL + 1))
    PASS=$((PASS + 1))
    log_pass "lib_self_test" "$1" "$2"
}

record_fail() {
    TOTAL=$((TOTAL + 1))
    FAIL=$((FAIL + 1))
    log_fail "lib_self_test" "$1" "$2"
}

assert_cmd() {
    local scenario="$1"
    local message="$2"
    shift 2
    if "$@"; then
        record_pass "${scenario}" "${message}"
    else
        record_fail "${scenario}" "${message}"
    fi
}

json_field_equals() {
    local json_line="$1"
    local path="$2"
    local expected="$3"
    python3 - "${json_line}" "${path}" "${expected}" <<'PY'
import json
import sys

doc = json.loads(sys.argv[1])
value = doc
for part in sys.argv[2].split("."):
    value = value[part]
if str(value) != sys.argv[3]:
    raise SystemExit(1)
PY
}

has_no_literal_newline() {
    [ "$(printf '%s' "$1" | wc -l)" = 0 ]
}

line="$(log_event info lib_self_test s1 "hello" '{"n":1}')"
assert_cmd "s1_log_event_emits_valid_json" "log_event emits parseable JSON" \
    json_field_equals "${line}" "data.n" "1"

special_message=$'quote " newline\nslash \\'
line="$(log_event info lib_self_test s2 "${special_message}" '{"ok":true}')"
assert_cmd "s2_log_event_escapes_special_chars" "log_event escapes special characters" \
    json_field_equals "${line}" "message" "${special_message}"

pretty_data=$'{\n  "nested": {\n    "ok": true\n  }\n}'
line="$(log_event info lib_self_test s2b "pretty data" "${pretty_data}")"
assert_cmd "s2b_log_event_keeps_one_json_line" "log_event compacts pretty data JSON" \
    has_no_literal_newline "${line}"
assert_cmd "s2b_log_event_keeps_one_json_line" "compacted data remains parseable" \
    json_field_equals "${line}" "data.nested.ok" "True"

summary_line="$(log_summary lib_self_test 3 2 1 "${WORK}" "bash tests/e2e/lib/lib_self_test.sh")"
assert_cmd "s3_log_summary_emits_summary_envelope" "log_summary emits summary envelope" \
    json_field_equals "${summary_line}" "summary.failed" "1"

SERVER_DIR="${WORK}/server"
STDOUT_LOG="${SERVER_DIR}/stdout.log"
STDERR_LOG="${SERVER_DIR}/stderr.log"
PID="$(spawn_server_with_storage \
    dummy \
    "${SERVER_DIR}" \
    "${SERVER_DIR}/storage" \
    "${SERVER_DIR}/db.sqlite3" \
    "${STDOUT_LOG}" \
    "${STDERR_LOG}" \
    -- \
    python3 -c 'import os,time; from pathlib import Path; Path(os.environ["STORAGE_ROOT"]).mkdir(parents=True, exist_ok=True); Path(os.environ["STORAGE_ROOT"], "marker.txt").write_text(os.environ["DATABASE_URL"], encoding="utf-8"); time.sleep(30)')"
sleep 0.2
assert_cmd "s4_spawn_server_isolates_storage_root" "spawned process is running" \
    kill -0 "${PID}"
assert_cmd "s4_spawn_server_isolates_storage_root" "storage root marker was written" \
    test -f "${SERVER_DIR}/storage/marker.txt"
stop_spawned_server "${PID}"

CORPUS_DIR="${WORK}/corpus"
seed_mixed_quality_corpus "${CORPUS_DIR}" 2 2
assert_cmd "s5_seed_corpus_writes_expected_files" "manifest exists" \
    test -f "${CORPUS_DIR}/manifest.jsonl"
assert_cmd "s5_seed_corpus_writes_expected_files" "manifest has four documents" \
    bash -c "[ \"$(wc -l <"${CORPUS_DIR}/manifest.jsonl")\" = 4 ]"

assert_cmd "s6_wait_for_with_satisfied_condition_returns_quickly" "satisfied condition returns success" \
    wait_for_condition 2 0.1 -- test -f "${CORPUS_DIR}/manifest.jsonl"

set +e
wait_for_condition 1 0.1 -- test -f "${WORK}/never-created"
timeout_rc=$?
set -e
if [ "${timeout_rc}" -ne 0 ]; then
    record_pass "s7_wait_for_timeout_returns_nonzero" "timeout condition returned non-zero"
else
    record_fail "s7_wait_for_timeout_returns_nonzero" "timeout condition unexpectedly succeeded"
fi

log_summary "lib_self_test" "${TOTAL}" "${PASS}" "${FAIL}" "${WORK}" "bash tests/e2e/lib/lib_self_test.sh"
if [ "${FAIL}" -ne 0 ]; then
    exit 1
fi
