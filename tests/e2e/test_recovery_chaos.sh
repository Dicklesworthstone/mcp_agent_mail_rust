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

capture_stdio_step() {
    local scenario="$1"
    local step="$2"
    local stdin_payload="$3"
    local stdio_binary
    stdio_binary="$(command -v am)"
    local request_frames=()
    mapfile -t request_frames <<<"${stdin_payload}"

    local step_dir="${CHAOS_LOG_DIR}/${scenario}/${step}"
    mkdir -p "${step_dir}"
    printf '%s\n' "${stdin_payload}" >"${step_dir}/stdin"

    {
        printf 'cd %q\nsource %q\nmapfile -t recovery_requests < %q\n' \
            "${E2E_PROJECT_ROOT}" "${SCRIPT_DIR}/../../scripts/e2e_lib.sh" "${step_dir}/stdin"
        printf 'HOME=%q STORAGE_ROOT=%q DATABASE_URL=%q AM_INTERFACE_MODE=cli HTTP_HOST=127.0.0.1 HTTP_PORT=%q E2E_STDIO_BINARY=%q E2E_STDIO_DEFAULT_ENTRYPOINT=0 E2E_STDIO_TIMEOUT_SECONDS=20 E2E_STDIO_STDERR_PATH=%q e2e_stdio_session %q "${recovery_requests[@]}"\n' \
            "${SCENARIO_HOME}" \
            "${SCENARIO_STORAGE}" \
            "${SCENARIO_DB_URL}" \
            "${SCENARIO_PORT}" \
            "${stdio_binary}" \
            "${step_dir}/server.stderr" \
            "${SCENARIO_DB}"
    } >"${step_dir}/replay.txt"

    # The shared client waits for initialize, sends initialized, then waits for
    # each dependent request's response before sending the next request.
    set +e
    HOME="${SCENARIO_HOME}" \
        STORAGE_ROOT="${SCENARIO_STORAGE}" \
        DATABASE_URL="${SCENARIO_DB_URL}" \
        AM_INTERFACE_MODE=cli \
        HTTP_HOST=127.0.0.1 \
        HTTP_PORT="${SCENARIO_PORT}" \
        E2E_STDIO_BINARY="${stdio_binary}" \
        E2E_STDIO_DEFAULT_ENTRYPOINT=0 \
        E2E_STDIO_TIMEOUT_SECONDS=20 \
        E2E_STDIO_STDERR_PATH="${step_dir}/server.stderr" \
        e2e_stdio_session "${SCENARIO_DB}" "${request_frames[@]}" \
        >"${step_dir}/stdout" 2>"${step_dir}/stderr"
    local rc=$?
    set -e

    printf '%s\n' "${rc}" >"${step_dir}/exit"
    emit_chaos_event "info" "${scenario}" "stdio step completed" "{\"step\":\"${step}\",\"exit\":${rc},\"stdin\":\"${step_dir}/stdin\",\"stdout\":\"${step_dir}/stdout\",\"stderr\":\"${step_dir}/stderr\",\"server_stderr\":\"${step_dir}/server.stderr\"}"
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
        return 1
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
    {"jsonrpc": "2.0", "method": "notifications/initialized"},
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
    capture_stdio_step "${scenario}" "seed_stdio" "${requests}"
    assert_step_exit "${scenario}" "seed_stdio" "0"
    if [ "$(step_exit "${scenario}" "seed_stdio")" != "0" ]; then
        return 1
    fi
    if python3 - "$(step_stdout "${scenario}" "seed_stdio")" <<'PY'
import json
import sys

responses = {}
with open(sys.argv[1], encoding="utf-8") as handle:
    for line in handle:
        response = json.loads(line)
        if "id" not in response:
            continue
        response_id = response["id"]
        assert response_id not in responses, f"duplicate seed response {response_id}"
        responses[response_id] = response

for response_id in (1, 10, 11, 12):
    assert response_id in responses, f"missing seed response {response_id}"
    response = responses[response_id]
    assert "error" not in response, f"seed request {response_id} failed: {response}"
    result = response.get("result")
    assert isinstance(result, dict), f"seed request {response_id} has no result: {response}"
    assert not result.get("isError", False), f"seed tool {response_id} failed: {result}"
PY
    then
        e2e_pass "${scenario} seed initialize, project, agent and send responses succeeded"
    else
        e2e_fail "${scenario} seed stdio returned an unsuccessful response"
        return 1
    fi
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

# N2 (br-bvq1x.14.2): the reliability snapshot must aggregate every
# reliability-epic diagnostic surface in ONE file — runtime identity
# (J2/J3), host pressure (J1), TUI/runtime loop heartbeats (I1/I2), the
# unified process-owner model (I4), and the fs-based mailbox-ownership /
# lock state (D1) — each documented in the snapshot's field_schema. This
# is the e2e proof that the capture bundle is complete and self-describing.
assert_support_bundle_reliability_snapshot() {
    local scenario="$1"
    local label="$2"
    local manifest snapshot
    manifest="$(latest_support_manifest)"
    if [ -z "${manifest}" ]; then
        e2e_fail "${scenario}/${label} reliability snapshot: bundle manifest exists"
        return 0
    fi
    snapshot="$(dirname "${manifest}")/reports/reliability-snapshot.json"
    if [ ! -f "${snapshot}" ]; then
        e2e_fail "${scenario}/${label} reliability snapshot file exists"
        return 0
    fi
    e2e_copy_artifact "${snapshot}" "recovery_chaos/${scenario}/${label}_reliability_snapshot.json"
    if python3 - "${snapshot}" <<'PY'
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    snap = json.load(handle)

required = [
    "runtime_identity",
    "host",
    "tui_liveness",
    "process_owner",
    "process_owner_divergences",
    "mailbox_ownership",
]
schema = snap.get("field_schema", {})
for section in required:
    if section not in snap:
        print(f"missing section: {section}", file=sys.stderr)
        sys.exit(1)
    if section not in schema:
        print(f"field_schema does not document: {section}", file=sys.stderr)
        sys.exit(1)

# I4: the five process-owner dimensions must all be present.
po = snap.get("process_owner", {})
for dim in ("expected_service", "actual_processes", "port", "db_path"):
    if dim not in po:
        print(f"process_owner missing dimension: {dim}", file=sys.stderr)
        sys.exit(1)
if not isinstance(snap.get("process_owner_divergences"), list):
    print("process_owner_divergences is not a list", file=sys.stderr)
    sys.exit(1)
# D1: fs-based lock surface exposes a disposition.
if "disposition" not in snap.get("mailbox_ownership", {}):
    print("mailbox_ownership missing disposition", file=sys.stderr)
    sys.exit(1)
# I1/I2: liveness probe reports an explicit source.
if "source" not in snap.get("tui_liveness", {}):
    print("tui_liveness missing source", file=sys.stderr)
    sys.exit(1)
sys.exit(0)
PY
    then
        e2e_pass "${scenario}/${label} reliability snapshot aggregates J1/J2/J3/I1/I4/D1 with documented field_schema"
    else
        e2e_fail "${scenario}/${label} reliability snapshot missing a reliability surface or field_schema entry"
    fi
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

assert_sidecar_evidence_preserved() {
    local scenario="$1"
    local step="$2"
    local expect_source_refusal="$3"
    if python3 - "${CHAOS_LOG_DIR}/${scenario}" "${step}" "${SCENARIO_DB}" "${expect_source_refusal}" <<'PY'
import hashlib
import json
from pathlib import Path
import sys

scenario_dir = Path(sys.argv[1])
step, db_path, expect_source_refusal = sys.argv[2], Path(sys.argv[3]), sys.argv[4] == "true"
before = scenario_dir / "sidecar-before"
step_dir = scenario_dir / step
manifest = json.loads((scenario_dir / f"{step}_manifest.json").read_text())
source = manifest["source"]
bundle = Path(source["storage_root"]) / "doctor" / "forensics" / source["db_family"] / manifest["bundle_name"]

def assert_captured(kind):
    expected = (before / kind).read_bytes()
    entry = manifest["artifacts"]["sqlite"][kind]
    assert entry["status"] == "captured", entry
    assert entry["bytes"] == len(expected), entry
    assert entry["sha256"] == hashlib.sha256(expected).hexdigest(), entry
    assert (bundle / entry["path"]).read_bytes() == expected, f"captured {kind} bytes changed"

assert_captured("db")
assert manifest["artifacts"]["sqlite"]["wal"]["status"] == "missing", manifest
active_wal = Path(f"{db_path}-wal")
assert not active_wal.exists() and not active_wal.is_symlink(), "damaged WAL remained active"
quarantines = list(db_path.parent.glob(f"{db_path.name}-wal.startup-quarantine-*"))
assert len(quarantines) == 1, quarantines
assert quarantines[0].read_bytes() == (before / "wal").read_bytes(), "quarantine lost WAL bytes"
assert str(quarantines[0]) in (step_dir / "stdout").read_text(), "quarantine path was not reported"

if expect_source_refusal:
    assert (step_dir / "exit").read_text().strip() == "1", "unsafe source was accepted"
    error = (step_dir / "stderr").read_text()
    assert "cannot override source safety" in error, error
    assert "rollback journal is attached to the live database family" in error, error
    assert db_path.read_bytes() == (before / "db").read_bytes(), "refusal changed main database"
    assert Path(f"{db_path}-journal").read_bytes() == (before / "journal").read_bytes(), "refusal changed rollback journal"
    assert_captured("journal")
PY
    then
        e2e_pass "${scenario}/${step} retains exact database and sidecar evidence"
    else
        e2e_fail "${scenario}/${step} changed source bytes or lost sidecar evidence"
        return 1
    fi
}

assert_repaired_message_state_preserved() {
    local scenario="$1"
    if python3 - "${CHAOS_LOG_DIR}/${scenario}/sidecar-before/db" "${SCENARIO_DB}" <<'PY'
from pathlib import Path
import sqlite3
import sys

def message_state(path):
    with sqlite3.connect(Path(path).resolve().as_uri() + "?mode=ro", uri=True) as conn:
        messages = conn.execute("SELECT * FROM messages ORDER BY id").fetchall()
        recipients = conn.execute("SELECT * FROM message_recipients ORDER BY message_id, agent_id, kind").fetchall()
    return messages, recipients

expected = message_state(sys.argv[1])
assert expected[0], "fixture had no persisted messages"
assert message_state(sys.argv[2]) == expected, "repair changed message or recipient state"
PY
    then
        e2e_pass "${scenario} repair preserves exact message and recipient rows"
    else
        e2e_fail "${scenario} repair changed message or recipient rows"
        return 1
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
assert_support_bundle_reliability_snapshot "${SCENARIO}" "doctor_support_bundle"

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

# Scenario 4: a legacy archive without a checkpoint requires explicit state-loss acceptance.
SCENARIO="reconstruct_malformed_sqlite_from_archive"
begin_recovery_scenario "${SCENARIO}" "reconstruct"
seed_mailbox "${SCENARIO}"
seed_canonical_archive_message "${SCENARIO}"
# Native seeding now publishes checkpoints. Retain that evidence separately to
# reproduce an older archive containing only profiles and message content.
if [ -d "${SCENARIO_STORAGE}/recovery-state" ]; then
    mv -- "${SCENARIO_STORAGE}/recovery-state" "${CHAOS_LOG_DIR}/${SCENARIO}/retained-recovery-state"
fi
# Retain the intact seed family outside recovery discovery. This scenario models
# an unavailable database, while scenarios 5 and 6 exercise live sidecar damage.
mkdir -p "${CHAOS_LOG_DIR}/${SCENARIO}/pre-corruption-family"
for family_file in "${SCENARIO_DB}"*; do
    if [ -f "${family_file}" ] || [ -L "${family_file}" ]; then
        mv -- "${family_file}" "${CHAOS_LOG_DIR}/${SCENARIO}/pre-corruption-family/"
    fi
done
printf 'not-a-sqlite-database\n' >"${SCENARIO_DB}"
cp -- "${SCENARIO_DB}" "${CHAOS_LOG_DIR}/${SCENARIO}/corrupt-source-before.sqlite3"
capture_step "${SCENARIO}" "doctor_reconstruct_refused" am doctor reconstruct --yes --json
assert_step_exit "${SCENARIO}" "doctor_reconstruct_refused" "1"
if python3 - "$(step_stdout "${SCENARIO}" "doctor_reconstruct_refused")" "${SCENARIO_DB}" "${CHAOS_LOG_DIR}/${SCENARIO}/corrupt-source-before.sqlite3" <<'PY'
import json
from pathlib import Path
import sys

output_path, db_path, before_path = map(Path, sys.argv[1:4])
data = json.loads(output_path.read_text(encoding="utf-8"))
assert data["status"] == "refused", data
assert data["salvage"]["status"] == "warn", data
assert data["salvage_outcome"] == "archive_only", data
assert data["recovery_checkpoint_present"] is False, data
assert data["archived_receipts"] == 0, data
assert data["archived_contacts"] == 0, data
assert data["delivery_cursor_reset_floor"] is None, data
assert isinstance(data["recovery_state_limitations"], list), data
assert data["requires_lossy_recovery"] is True, data
assert data["allow_state_loss"] is False, data
assert data["actions_taken"] == 0, data
assert data["state_loss_risks"], data
assert "--allow-state-loss" in data["detail"], data
assert Path(data["candidate_path"]).is_file(), data
assert db_path.read_bytes() == before_path.read_bytes(), "refusal changed corrupt source"
PY
then
    e2e_pass "archive-only recovery refuses with retained candidate and unchanged source"
else
    e2e_fail "archive-only refusal lost its structured disclosure or source evidence"
fi
capture_step "${SCENARIO}" "doctor_reconstruct" am doctor reconstruct --yes --allow-state-loss --json
assert_step_exit "${SCENARIO}" "doctor_reconstruct" "0"
assert_file_contains_any "reconstruct output identifies archive rebuild" "$(step_stdout "${SCENARIO}" "doctor_reconstruct")" "reconstruct" "recovered" "Reconstructed"
assert_file_contains "explicit recovery warns of state loss" "$(step_stderr "${SCENARIO}" "doctor_reconstruct")" "WARNING: --allow-state-loss"
if python3 - "$(step_stdout "${SCENARIO}" "doctor_reconstruct")" "${SCENARIO_DB}" "${CHAOS_LOG_DIR}/${SCENARIO}/corrupt-source-before.sqlite3" <<'PY'
import json
from pathlib import Path
import sys

output_path, db_path, before_path = map(Path, sys.argv[1:4])
data = json.loads(output_path.read_text(encoding="utf-8"))
assert data["status"] == "ok", data
assert data["salvage"]["status"] == "warn", data
assert data["salvage_outcome"] == "archive_only", data
assert data["recovery_checkpoint_present"] is False, data
assert data["archived_receipts"] == 0, data
assert data["archived_contacts"] == 0, data
assert data["delivery_cursor_reset_floor"] is None, data
assert isinstance(data["recovery_state_limitations"], list), data
assert data["requires_lossy_recovery"] is True, data
assert data["allow_state_loss"] is True, data
assert data["state_loss_risks"], data
assert data["recovered"]["messages"] > 0, data
before = before_path.read_bytes()
assert any(
    path.is_file() and path.read_bytes() == before
    for path in db_path.parent.glob(f"{db_path.name}.corrupt-*")
), "accepted recovery did not retain the corrupt source"
PY
then
    e2e_pass "explicit recovery discloses loss and preserves corrupt source quarantine"
else
    e2e_fail "explicit recovery lost its disclosure or corrupt source quarantine"
fi
assert_sql_eq "reconstructed database quick_check" "PRAGMA quick_check;" "ok"
assert_sql_gt_zero "reconstructed database restored messages" "SELECT COUNT(*) FROM messages;"
capture_step "${SCENARIO}" "doctor_support_bundle" am doctor support-bundle --stdout-log "$(step_stdout "${SCENARIO}" "doctor_reconstruct")" --stderr-log "$(step_stderr "${SCENARIO}" "doctor_reconstruct")" --redact-subjects --json
assert_step_exit "${SCENARIO}" "doctor_support_bundle" "0"
copy_latest_support_manifest "${SCENARIO}" "doctor_support_bundle"
assert_support_bundle_observed_command "${SCENARIO}" "doctor_support_bundle" "reconstruct"
assert_support_manifest_safe_sharing_limits "${SCENARIO}" "doctor_support_bundle"
assert_support_bundle_reliability_snapshot "${SCENARIO}" "doctor_support_bundle"

# Scenario 5: malformed sidecars retain evidence and block unsafe source admission.
SCENARIO="forensics_captures_live_sidecars"
begin_recovery_scenario "${SCENARIO}" "refuse_unsafe_journal_then_repair"
seed_mailbox "${SCENARIO}"
mkdir -p "${CHAOS_LOG_DIR}/${SCENARIO}/sidecar-before"
cp -- "${SCENARIO_DB}" "${CHAOS_LOG_DIR}/${SCENARIO}/sidecar-before/db"
printf 'WAL-sidecar-chaos\n' >"${SCENARIO_DB}-wal"
printf 'journal-sidecar-chaos\n' >"${SCENARIO_DB}-journal"
cp -- "${SCENARIO_DB}-wal" "${CHAOS_LOG_DIR}/${SCENARIO}/sidecar-before/wal"
cp -- "${SCENARIO_DB}-journal" "${CHAOS_LOG_DIR}/${SCENARIO}/sidecar-before/journal"
capture_step "${SCENARIO}" "doctor_repair" am doctor repair --yes
assert_step_exit "${SCENARIO}" "doctor_repair" "1"
copy_latest_forensic_manifest "${SCENARIO}" "doctor_repair"
assert_manifest_sqlite_status "${SCENARIO}" "doctor_repair" "db" "captured"
assert_manifest_sqlite_status "${SCENARIO}" "doctor_repair" "wal" "missing"
assert_manifest_sqlite_status "${SCENARIO}" "doctor_repair" "journal" "captured"
assert_sidecar_evidence_preserved "${SCENARIO}" "doctor_repair" "true"
# The operator retains the invalid journal outside the live family only after
# proving the refusal preserved it. The healthy main database remains in place.
mv -- "${SCENARIO_DB}-journal" "${CHAOS_LOG_DIR}/${SCENARIO}/retained-invalid-journal"
capture_step "${SCENARIO}" "doctor_repair_after_journal_retained" am doctor repair --yes
assert_step_exit "${SCENARIO}" "doctor_repair_after_journal_retained" "0"
e2e_assert_eq "retained invalid journal still matches the injected bytes" \
    "$(e2e_sha256 "${CHAOS_LOG_DIR}/${SCENARIO}/sidecar-before/journal")" \
    "$(e2e_sha256 "${CHAOS_LOG_DIR}/${SCENARIO}/retained-invalid-journal")"
assert_sql_eq "repair after journal retention quick_check" "PRAGMA quick_check;" "ok"
assert_repaired_message_state_preserved "${SCENARIO}"

# Scenario 6: an invalid 32-byte WAL header is reported read-only, then quarantined.
SCENARIO="header_only_wal_sidecar"
begin_recovery_scenario "${SCENARIO}" "repair"
seed_mailbox "${SCENARIO}"
mkdir -p "${CHAOS_LOG_DIR}/${SCENARIO}/sidecar-before"
cp -- "${SCENARIO_DB}" "${CHAOS_LOG_DIR}/${SCENARIO}/sidecar-before/db"
python3 - "${SCENARIO_DB}-wal" <<'PY'
import pathlib
import sys
pathlib.Path(sys.argv[1]).write_bytes(b"\x37\x7f\x06\x82" + b"\x00" * 28)
PY
cp -- "${SCENARIO_DB}-wal" "${CHAOS_LOG_DIR}/${SCENARIO}/sidecar-before/wal"
capture_step "${SCENARIO}" "doctor_check" am doctor check --json
assert_step_exit_any "${SCENARIO}" "doctor_check" "0" "1"
assert_doctor_check_status "${SCENARIO}" "doctor_check" "db_file_sanity" "fail"
e2e_assert_eq "doctor check preserves the invalid WAL bytes" \
    "$(e2e_sha256 "${CHAOS_LOG_DIR}/${SCENARIO}/sidecar-before/wal")" \
    "$(e2e_sha256 "${SCENARIO_DB}-wal")"
e2e_assert_eq "doctor check preserves the main database bytes" \
    "$(e2e_sha256 "${CHAOS_LOG_DIR}/${SCENARIO}/sidecar-before/db")" \
    "$(e2e_sha256 "${SCENARIO_DB}")"
capture_step "${SCENARIO}" "doctor_repair" am doctor repair --yes
assert_step_exit "${SCENARIO}" "doctor_repair" "0"
copy_latest_forensic_manifest "${SCENARIO}" "doctor_repair"
assert_manifest_sqlite_status "${SCENARIO}" "doctor_repair" "db" "captured"
assert_manifest_sqlite_status "${SCENARIO}" "doctor_repair" "wal" "missing"
assert_sidecar_evidence_preserved "${SCENARIO}" "doctor_repair" "false"
assert_sql_eq "header-only WAL repair quick_check" "PRAGMA quick_check;" "ok"
assert_repaired_message_state_preserved "${SCENARIO}"

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
