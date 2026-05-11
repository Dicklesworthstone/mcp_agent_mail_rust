#!/usr/bin/env bash
# test_recovery_chaos.sh - No-mock mailbox recovery chaos harness.
# @tags: slow, recovery, doctor, no-mock

E2E_SUITE="recovery_chaos"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"
# shellcheck source=lib/structured_logging.sh
source "${SCRIPT_DIR}/lib/structured_logging.sh"

e2e_init_artifacts
e2e_banner "Recovery Chaos E2E Suite (br-oci92.4)"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

WORK="$(e2e_mktemp "e2e_recovery_chaos")"
CHAOS_LOG_DIR="${E2E_ARTIFACT_DIR}/recovery_chaos"
CHAOS_EVENTS="${CHAOS_LOG_DIR}/events.jsonl"
mkdir -p "${CHAOS_LOG_DIR}"
: >"${CHAOS_EVENTS}"

SCENARIO_ROOT=""
SCENARIO_HOME=""
SCENARIO_STORAGE=""
SCENARIO_DB=""
SCENARIO_DB_URL=""
SCENARIO_PROJECT=""
SCENARIO_PORT=""

emit_chaos_event() {
    local level="$1"
    local scenario="$2"
    local message="$3"
    local data="${4:-{}}"
    log_event "${level}" "recovery_chaos" "${scenario}" "${message}" "${data}" >>"${CHAOS_EVENTS}"
}

begin_recovery_scenario() {
    local scenario="$1"
    local decision="$2"

    e2e_case_banner "${scenario}"
    SCENARIO_ROOT="${WORK}/${scenario}"
    SCENARIO_HOME="${SCENARIO_ROOT}/home"
    SCENARIO_STORAGE="${SCENARIO_ROOT}/storage"
    SCENARIO_DB="${SCENARIO_ROOT}/storage.sqlite3"
    SCENARIO_DB_URL="sqlite:///${SCENARIO_DB}"
    SCENARIO_PROJECT="${SCENARIO_ROOT}/project"
    SCENARIO_PORT=$(( 18000 + (_E2E_TOTAL * 37) + (_E2E_RNG_STATE % 1000) ))
    mkdir -p "${SCENARIO_HOME}" "${SCENARIO_STORAGE}" "${SCENARIO_PROJECT}" "${CHAOS_LOG_DIR}/${scenario}"

    python3 - "${CHAOS_LOG_DIR}/${scenario}/state.json" <<PY
import json
import sys
state = {
    "scenario": "${scenario}",
    "expected_decision": "${decision}",
    "home": "${SCENARIO_HOME}",
    "storage_root": "${SCENARIO_STORAGE}",
    "database_url": "${SCENARIO_DB_URL}",
    "project": "${SCENARIO_PROJECT}",
    "http_port": ${SCENARIO_PORT},
}
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    json.dump(state, handle, indent=2, sort_keys=True)
    handle.write("\n")
PY
    emit_chaos_event "info" "${scenario}" "scenario initialized" "{\"expected_decision\":\"${decision}\",\"state_path\":\"${CHAOS_LOG_DIR}/${scenario}/state.json\"}"
}

capture_step() {
    local scenario="$1"
    local step="$2"
    shift 2
    local cmd_argv=("$@")

    local step_dir="${CHAOS_LOG_DIR}/${scenario}/${step}"
    mkdir -p "${step_dir}"

    printf 'cd %q && HOME=%q STORAGE_ROOT=%q DATABASE_URL=%q AM_INTERFACE_MODE=cli HTTP_HOST=127.0.0.1 HTTP_PORT=%q %q' \
        "${E2E_PROJECT_ROOT}" \
        "${SCENARIO_HOME}" \
        "${SCENARIO_STORAGE}" \
        "${SCENARIO_DB_URL}" \
        "${SCENARIO_PORT}" \
        "${cmd_argv[0]}" >"${step_dir}/replay.txt"
    for arg in "${cmd_argv[@]:1}"; do
        printf ' %q' "${arg}" >>"${step_dir}/replay.txt"
    done
    printf '\n' >>"${step_dir}/replay.txt"

    set +e
    HOME="${SCENARIO_HOME}" \
        STORAGE_ROOT="${SCENARIO_STORAGE}" \
        DATABASE_URL="${SCENARIO_DB_URL}" \
        AM_INTERFACE_MODE=cli \
        HTTP_HOST=127.0.0.1 \
        HTTP_PORT="${SCENARIO_PORT}" \
        "${cmd_argv[@]}" >"${step_dir}/stdout" 2>"${step_dir}/stderr"
    local rc=$?
    set -e

    printf '%s\n' "${rc}" >"${step_dir}/exit"
    emit_chaos_event "info" "${scenario}" "step completed" "{\"step\":\"${step}\",\"exit\":${rc},\"stdout\":\"${step_dir}/stdout\",\"stderr\":\"${step_dir}/stderr\"}"
    return 0
}

capture_stdin_step() {
    local scenario="$1"
    local step="$2"
    local stdin_payload="$3"
    shift 3
    local cmd_argv=("$@")

    local step_dir="${CHAOS_LOG_DIR}/${scenario}/${step}"
    mkdir -p "${step_dir}"
    printf '%s\n' "${stdin_payload}" >"${step_dir}/stdin"

    {
        printf 'cd %q && HOME=%q STORAGE_ROOT=%q DATABASE_URL=%q AM_INTERFACE_MODE=cli HTTP_HOST=127.0.0.1 HTTP_PORT=%q ' \
            "${E2E_PROJECT_ROOT}" \
            "${SCENARIO_HOME}" \
            "${SCENARIO_STORAGE}" \
            "${SCENARIO_DB_URL}" \
            "${SCENARIO_PORT}"
        printf 'cat %q | %q' "${step_dir}/stdin" "${cmd_argv[0]}"
        for arg in "${cmd_argv[@]:1}"; do
            printf ' %q' "${arg}"
        done
        printf '\n'
    } >"${step_dir}/replay.txt"

    set +e
    HOME="${SCENARIO_HOME}" \
        STORAGE_ROOT="${SCENARIO_STORAGE}" \
        DATABASE_URL="${SCENARIO_DB_URL}" \
        AM_INTERFACE_MODE=cli \
        HTTP_HOST=127.0.0.1 \
        HTTP_PORT="${SCENARIO_PORT}" \
        "${cmd_argv[@]}" >"${step_dir}/stdout" 2>"${step_dir}/stderr" <<<"${stdin_payload}"
    local rc=$?
    set -e

    printf '%s\n' "${rc}" >"${step_dir}/exit"
    emit_chaos_event "info" "${scenario}" "stdio step completed" "{\"step\":\"${step}\",\"exit\":${rc},\"stdin\":\"${step_dir}/stdin\",\"stdout\":\"${step_dir}/stdout\",\"stderr\":\"${step_dir}/stderr\"}"
    return 0
}

step_stdout() {
    printf '%s/recovery_chaos/%s/%s/stdout\n' "${E2E_ARTIFACT_DIR}" "$1" "$2"
}

step_stderr() {
    printf '%s/recovery_chaos/%s/%s/stderr\n' "${E2E_ARTIFACT_DIR}" "$1" "$2"
}

step_exit() {
    cat "${CHAOS_LOG_DIR}/$1/$2/exit"
}

assert_step_exit() {
    local scenario="$1"
    local step="$2"
    local expected="$3"
    local actual
    actual="$(step_exit "${scenario}" "${step}")"
    e2e_assert_eq "${scenario}/${step} exit" "${expected}" "${actual}"
}

assert_step_exit_any() {
    local scenario="$1"
    local step="$2"
    shift 2
    local actual
    actual="$(step_exit "${scenario}" "${step}")"
    for expected in "$@"; do
        if [ "${actual}" = "${expected}" ]; then
            e2e_pass "${scenario}/${step} exit=${actual}"
            return 0
        fi
    done
    e2e_fail "${scenario}/${step} unexpected exit=${actual}"
}

assert_file_contains() {
    local label="$1"
    local path="$2"
    local needle="$3"
    if grep -Fq "${needle}" "${path}" 2>/dev/null; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}: missing ${needle} in ${path}"
    fi
}

assert_file_contains_any() {
    local label="$1"
    local path="$2"
    shift 2
    local needle
    for needle in "$@"; do
        if grep -Fq "${needle}" "${path}" 2>/dev/null; then
            e2e_pass "${label}"
            return 0
        fi
    done
    e2e_fail "${label}: none of the expected markers were present in ${path}"
}

assert_step_output_contains_any() {
    local label="$1"
    local scenario="$2"
    local step="$3"
    shift 3
    local combined="${CHAOS_LOG_DIR}/${scenario}/${step}/combined.txt"
    cat "$(step_stdout "${scenario}" "${step}")" "$(step_stderr "${scenario}" "${step}")" >"${combined}"
    assert_file_contains_any "${label}" "${combined}" "$@"
}

assert_doctor_check_status() {
    local scenario="$1"
    local step="$2"
    local check_name="$3"
    local expected_status="$4"
    local stdout_path
    stdout_path="$(step_stdout "${scenario}" "${step}")"
    if python3 - "${stdout_path}" "${check_name}" "${expected_status}" <<'PY'
import json
import sys
path, check_name, expected = sys.argv[1:4]
with open(path, encoding="utf-8") as handle:
    data = json.load(handle)
for check in data.get("checks", []):
    if check.get("check") == check_name and check.get("status") == expected:
        sys.exit(0)
sys.exit(1)
PY
    then
        e2e_pass "${scenario}/${step} ${check_name}=${expected_status}"
    else
        e2e_fail "${scenario}/${step} missing ${check_name}=${expected_status}"
    fi
}

sql_scalar() {
    local sql="$1"
    python3 - "${SCENARIO_DB}" "${sql}" <<'PY'
import sqlite3
import sys
db_path, sql = sys.argv[1:3]
with sqlite3.connect(db_path) as conn:
    row = conn.execute(sql).fetchone()
if row is not None and len(row) > 0 and row[0] is not None:
    print(row[0])
PY
}

sql_exec() {
    local sql="$1"
    python3 - "${SCENARIO_DB}" "${sql}" <<'PY'
import sqlite3
import sys
db_path, sql = sys.argv[1:3]
with sqlite3.connect(db_path) as conn:
    conn.executescript(sql)
    conn.commit()
PY
}

assert_sql_eq() {
    local label="$1"
    local sql="$2"
    local expected="$3"
    local actual
    actual="$(sql_scalar "${sql}")"
    e2e_assert_eq "${label}" "${expected}" "${actual}"
}

assert_sql_gt_zero() {
    local label="$1"
    local sql="$2"
    local actual
    actual="$(sql_scalar "${sql}")"
    if [ "${actual:-0}" -gt 0 ] 2>/dev/null; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}: expected > 0, got ${actual:-empty}"
    fi
}

seed_mailbox() {
    local scenario="$1"
    local requests
    requests="$(python3 - "${SCENARIO_PROJECT}" "${scenario}" <<'PY'
import json
import sys
project, scenario = sys.argv[1:3]
requests = [
    {
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "recovery-chaos", "version": "1.0"},
        },
    },
    {
        "jsonrpc": "2.0",
        "id": 10,
        "method": "tools/call",
        "params": {
            "name": "ensure_project",
            "arguments": {"human_key": project},
        },
    },
    {
        "jsonrpc": "2.0",
        "id": 11,
        "method": "tools/call",
        "params": {
            "name": "register_agent",
            "arguments": {
                "project_key": project,
                "program": "test",
                "model": "test",
                "name": "BlueLake",
            },
        },
    },
    {
        "jsonrpc": "2.0",
        "id": 12,
        "method": "tools/call",
        "params": {
            "name": "send_message",
            "arguments": {
                "project_key": project,
                "sender_name": "BlueLake",
                "to": ["BlueLake"],
                "subject": "recovery-chaos-seed",
                "body_md": f"seed message for recovery chaos {scenario}",
            },
        },
    },
]
print("\n".join(json.dumps(request, separators=(",", ":")) for request in requests))
PY
)"
    capture_stdin_step "${scenario}" "seed_stdio" "${requests}" timeout 20s am serve-stdio
    assert_step_exit "${scenario}" "seed_stdio" "0"
    assert_file_contains "${scenario} seed stdio got send response" "$(step_stdout "${scenario}" "seed_stdio")" '"id":12'
    assert_sql_gt_zero "${scenario} seeded at least one message" "SELECT COUNT(*) FROM messages;"
}

seed_canonical_archive_message() {
    local scenario="$1"
    local project_dir
    project_dir="$(find "${SCENARIO_STORAGE}/projects" -mindepth 1 -maxdepth 1 -type d 2>/dev/null | sort | head -n 1)"
    if [ -z "${project_dir}" ]; then
        e2e_fail "${scenario} has archive project directory"
        return 0
    fi

    mkdir -p "${project_dir}/messages/2026/03"
    python3 - "${project_dir}/messages/2026/03/20260322T000001Z__9001.md" "${scenario}" <<'PY'
import json
import sys

path, scenario = sys.argv[1:3]
envelope = {
    "id": 9001,
    "from": "BlueLake",
    "to": ["BlueLake"],
    "cc": [],
    "bcc": [],
    "subject": f"recovery archive seed {scenario}",
    "thread_id": "recovery-chaos-archive",
    "importance": "normal",
    "ack_required": False,
    "created_ts": "2026-03-22T00:00:01Z",
    "attachments": [],
}
with open(path, "w", encoding="utf-8") as handle:
    handle.write("---json\n")
    handle.write(json.dumps(envelope, separators=(",", ":")))
    handle.write("\n---\n")
    handle.write(f"Archive-backed recovery seed for {scenario}.\n")
PY
    assert_file_contains "${scenario} canonical archive message exists" "${project_dir}/messages/2026/03/20260322T000001Z__9001.md" "recovery archive seed"
}

insert_missing_message_recipient() {
    local agent_id
    agent_id="$(sql_scalar "SELECT id FROM agents ORDER BY id LIMIT 1;")"
    sql_exec "INSERT INTO message_recipients (message_id, agent_id, kind, read_ts, ack_ts) VALUES (999999, ${agent_id}, 'to', NULL, NULL);"
    assert_sql_eq "inserted repairable orphan recipient" "SELECT COUNT(*) FROM message_recipients WHERE message_id = 999999;" "1"
}

latest_forensic_manifest() {
    find "${SCENARIO_STORAGE}/doctor/forensics" -type f -name manifest.json 2>/dev/null | sort | tail -n 1
}

latest_support_manifest() {
    find "${SCENARIO_STORAGE}/doctor/support-bundles" -type f -name manifest.json 2>/dev/null | sort | tail -n 1
}

copy_latest_forensic_manifest() {
    local scenario="$1"
    local label="$2"
    local manifest
    manifest="$(latest_forensic_manifest)"
    if [ -z "${manifest}" ]; then
        e2e_fail "${scenario}/${label} forensic manifest exists"
        return 0
    fi
    e2e_pass "${scenario}/${label} forensic manifest exists"
    e2e_copy_artifact "${manifest}" "recovery_chaos/${scenario}/${label}_manifest.json"
}

copy_latest_support_manifest() {
    local scenario="$1"
    local label="$2"
    local manifest
    manifest="$(latest_support_manifest)"
    if [ -z "${manifest}" ]; then
        e2e_fail "${scenario}/${label} support manifest exists"
        return 0
    fi
    e2e_pass "${scenario}/${label} support manifest exists"
    e2e_copy_artifact "${manifest}" "recovery_chaos/${scenario}/${label}_support_manifest.json"
}

assert_support_bundle_observed_command() {
    local scenario="$1"
    local label="$2"
    local expected="$3"
    local manifest="${CHAOS_LOG_DIR}/${scenario}/${label}_support_manifest.json"
    if python3 - "${manifest}" "${expected}" <<'PY'
import json
import sys
path, expected = sys.argv[1:3]
with open(path, encoding="utf-8") as handle:
    manifest = json.load(handle)
if manifest.get("observed_recovery_command") != expected:
    sys.exit(1)
found = any(
    entry.get("path") == "reports/latest-forensic-manifest.json"
    and entry.get("source_path_class") == "raw_forensic_manifest"
    for entry in manifest.get("files", [])
)
if not found:
    sys.exit(1)
commands = manifest.get("replay_commands", [])
if "am doctor support-bundle --json" not in commands:
    sys.exit(1)
sys.exit(0)
PY
    then
        e2e_pass "${scenario}/${label} support bundle references ${expected} forensic decision"
    else
        e2e_fail "${scenario}/${label} support bundle missing ${expected} forensic decision"
    fi
}

assert_support_manifest_safe_sharing_limits() {
    local scenario="$1"
    local label="$2"
    local manifest="${CHAOS_LOG_DIR}/${scenario}/${label}_support_manifest.json"
    assert_file_contains "${scenario}/${label} support manifest omits raw sqlite" "${manifest}" "SQLite database and sidecars"
    assert_file_contains "${scenario}/${label} support manifest omits message bodies" "${manifest}" "message bodies and canonical message files"
    assert_file_contains "${scenario}/${label} support manifest omits attachments" "${manifest}" "attachment contents and attachment filenames"
}

assert_manifest_sqlite_status() {
    local scenario="$1"
    local label="$2"
    local kind="$3"
    local expected="$4"
    local manifest="${CHAOS_LOG_DIR}/${scenario}/${label}_manifest.json"
    if python3 - "${manifest}" "${kind}" "${expected}" <<'PY'
import json
import sys
path, kind, expected = sys.argv[1:4]
with open(path, encoding="utf-8") as handle:
    data = json.load(handle)
status = data.get("artifacts", {}).get("sqlite", {}).get(kind, {}).get("status")
sys.exit(0 if status == expected else 1)
PY
    then
        e2e_pass "${scenario}/${label} manifest ${kind}=${expected}"
    else
        e2e_fail "${scenario}/${label} manifest ${kind} did not equal ${expected}"
    fi
}

assert_manifest_has_reference() {
    local scenario="$1"
    local label="$2"
    local reference="$3"
    local manifest="${CHAOS_LOG_DIR}/${scenario}/${label}_manifest.json"
    if python3 - "${manifest}" "${reference}" <<'PY'
import json
import sys
path, reference = sys.argv[1:3]
with open(path, encoding="utf-8") as handle:
    data = json.load(handle)
refs = data.get("layout", {}).get("referenced_evidence", [])
sys.exit(0 if reference in refs else 1)
PY
    then
        e2e_pass "${scenario}/${label} manifest references ${reference}"
    else
        e2e_fail "${scenario}/${label} manifest missing ${reference}"
    fi
}

# Scenario 1: healthy isolated mailbox produces a real doctor database check.
SCENARIO="healthy_doctor_database_check"
begin_recovery_scenario "${SCENARIO}" "check"
seed_mailbox "${SCENARIO}"
capture_step "${SCENARIO}" "doctor_check" am doctor check --json
assert_step_exit_any "${SCENARIO}" "doctor_check" "0" "1"
assert_doctor_check_status "${SCENARIO}" "doctor_check" "database" "ok"
assert_doctor_check_status "${SCENARIO}" "doctor_check" "db_file_sanity" "ok"

# Scenario 2: explicit doctor repair removes repairable orphan recipient rows.
SCENARIO="repair_orphan_message_recipient"
begin_recovery_scenario "${SCENARIO}" "repair"
seed_mailbox "${SCENARIO}"
insert_missing_message_recipient
capture_step "${SCENARIO}" "doctor_check_before_repair" am doctor check --json
assert_step_exit_any "${SCENARIO}" "doctor_check_before_repair" "0" "1"
assert_file_contains "doctor check names orphaned recipients" "$(step_stdout "${SCENARIO}" "doctor_check_before_repair")" "orphaned message_recipients"
capture_step "${SCENARIO}" "doctor_repair" am doctor repair --yes
assert_step_exit "${SCENARIO}" "doctor_repair" "0"
assert_sql_eq "doctor repair removed missing-message recipient" "SELECT COUNT(*) FROM message_recipients WHERE message_id = 999999;" "0"
copy_latest_forensic_manifest "${SCENARIO}" "doctor_repair"
assert_manifest_sqlite_status "${SCENARIO}" "doctor_repair" "db" "captured"
assert_manifest_has_reference "${SCENARIO}" "doctor_repair" "archive_drift_report"
capture_step "${SCENARIO}" "doctor_support_bundle" am doctor support-bundle --stdout-log "$(step_stdout "${SCENARIO}" "doctor_repair")" --stderr-log "$(step_stderr "${SCENARIO}" "doctor_repair")" --redact-subjects --json
assert_step_exit "${SCENARIO}" "doctor_support_bundle" "0"
copy_latest_support_manifest "${SCENARIO}" "doctor_support_bundle"
assert_support_bundle_observed_command "${SCENARIO}" "doctor_support_bundle" "repair"
assert_support_manifest_safe_sharing_limits "${SCENARIO}" "doctor_support_bundle"

# Scenario 3: startup path chooses automatic repair, not reconstruct, for repairable FK drift.
SCENARIO="startup_auto_repair_orphan_recipient"
begin_recovery_scenario "${SCENARIO}" "startup_repair"
seed_mailbox "${SCENARIO}"
insert_missing_message_recipient
capture_step "${SCENARIO}" "serve_http_startup" \
    timeout 8s am serve-http --host 127.0.0.1 --port "${SCENARIO_PORT}" --no-auth --no-tui
assert_step_exit_any "${SCENARIO}" "serve_http_startup" "0" "124"
assert_step_output_contains_any "startup selected repair" "${SCENARIO}" "serve_http_startup" "running automatic repair"
assert_step_output_contains_any "startup completed repair" "${SCENARIO}" "serve_http_startup" "Automatic mailbox repair completed"
assert_sql_eq "startup repair removed missing-message recipient" "SELECT COUNT(*) FROM message_recipients WHERE message_id = 999999;" "0"

# Scenario 4: malformed SQLite with an archive is reconstructed from archive.
SCENARIO="reconstruct_malformed_sqlite_from_archive"
begin_recovery_scenario "${SCENARIO}" "reconstruct"
seed_mailbox "${SCENARIO}"
seed_canonical_archive_message "${SCENARIO}"
printf 'not-a-sqlite-database\n' >"${SCENARIO_DB}"
capture_step "${SCENARIO}" "doctor_reconstruct" am doctor reconstruct --yes --json
assert_step_exit "${SCENARIO}" "doctor_reconstruct" "0"
assert_file_contains_any "reconstruct output identifies archive rebuild" "$(step_stdout "${SCENARIO}" "doctor_reconstruct")" "reconstruct" "recovered" "Reconstructed"
assert_sql_eq "reconstructed database quick_check" "PRAGMA quick_check;" "ok"
assert_sql_gt_zero "reconstructed database restored messages" "SELECT COUNT(*) FROM messages;"
capture_step "${SCENARIO}" "doctor_support_bundle" am doctor support-bundle --stdout-log "$(step_stdout "${SCENARIO}" "doctor_reconstruct")" --stderr-log "$(step_stderr "${SCENARIO}" "doctor_reconstruct")" --redact-subjects --json
assert_step_exit "${SCENARIO}" "doctor_support_bundle" "0"
copy_latest_support_manifest "${SCENARIO}" "doctor_support_bundle"
assert_support_bundle_observed_command "${SCENARIO}" "doctor_support_bundle" "reconstruct"
assert_support_manifest_safe_sharing_limits "${SCENARIO}" "doctor_support_bundle"

# Scenario 5: repair forensics capture live SQLite sidecars before mutation.
SCENARIO="forensics_captures_live_sidecars"
begin_recovery_scenario "${SCENARIO}" "repair_forensics"
seed_mailbox "${SCENARIO}"
printf 'WAL-sidecar-chaos\n' >"${SCENARIO_DB}-wal"
printf 'journal-sidecar-chaos\n' >"${SCENARIO_DB}-journal"
capture_step "${SCENARIO}" "doctor_repair" am doctor repair --yes
assert_step_exit "${SCENARIO}" "doctor_repair" "0"
copy_latest_forensic_manifest "${SCENARIO}" "doctor_repair"
assert_manifest_sqlite_status "${SCENARIO}" "doctor_repair" "db" "captured"
assert_manifest_sqlite_status "${SCENARIO}" "doctor_repair" "wal" "captured"
assert_manifest_sqlite_status "${SCENARIO}" "doctor_repair" "journal" "captured"

# Scenario 6: header-only WAL sidecar is a real file-path chaos case and must not crash repair.
SCENARIO="header_only_wal_sidecar"
begin_recovery_scenario "${SCENARIO}" "repair"
seed_mailbox "${SCENARIO}"
python3 - "${SCENARIO_DB}-wal" <<'PY'
import pathlib
import sys
pathlib.Path(sys.argv[1]).write_bytes(b"\x37\x7f\x06\x82" + b"\x00" * 28)
PY
capture_step "${SCENARIO}" "doctor_check" am doctor check --json
assert_step_exit_any "${SCENARIO}" "doctor_check" "0" "1"
capture_step "${SCENARIO}" "doctor_repair" am doctor repair --yes
assert_step_exit "${SCENARIO}" "doctor_repair" "0"
copy_latest_forensic_manifest "${SCENARIO}" "doctor_repair"
assert_manifest_sqlite_status "${SCENARIO}" "doctor_repair" "wal" "captured"

# Scenario 7: archive drift evidence is materialized into the forensic bundle.
SCENARIO="archive_drift_forensic_reference"
begin_recovery_scenario "${SCENARIO}" "repair_forensics"
seed_mailbox "${SCENARIO}"
sql_exec "DELETE FROM messages WHERE id = (SELECT MIN(id) FROM messages);"
capture_step "${SCENARIO}" "doctor_repair" am doctor repair --yes
assert_step_exit "${SCENARIO}" "doctor_repair" "0"
copy_latest_forensic_manifest "${SCENARIO}" "doctor_repair"
assert_manifest_has_reference "${SCENARIO}" "doctor_repair" "archive_drift_report"
assert_file_contains "archive drift report path included" "${CHAOS_LOG_DIR}/${SCENARIO}/doctor_repair_manifest.json" "archive-drift-report.json"

# Scenario 8: stale recovery locks are reported as stale, not treated as live ownership.
SCENARIO="stale_recovery_lock"
begin_recovery_scenario "${SCENARIO}" "check"
seed_mailbox "${SCENARIO}"
printf '999999\n' >"${SCENARIO_DB}.recovery.lock"
capture_step "${SCENARIO}" "doctor_check" am doctor check --json
assert_step_exit_any "${SCENARIO}" "doctor_check" "0" "1"
assert_doctor_check_status "${SCENARIO}" "doctor_check" "recovery_lock" "ok"
assert_file_contains_any "stale recovery lock reported" "$(step_stdout "${SCENARIO}" "doctor_check")" "Stale recovery lock" "recovery lock"

log_summary "recovery_chaos" 8 "${_E2E_PASS}" "${_E2E_FAIL}" "${CHAOS_LOG_DIR}" "bash tests/e2e/test_recovery_chaos.sh" >>"${CHAOS_EVENTS}"
e2e_save_artifact "recovery_chaos/events.jsonl" "$(cat "${CHAOS_EVENTS}")"
e2e_summary
