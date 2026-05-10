#!/usr/bin/env bash
# test_banner_json.sh - E2E coverage for startup-state JSON in the rich banner.

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="banner_json"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"
# shellcheck source=lib/structured_logging.sh
source "${SCRIPT_DIR}/lib/structured_logging.sh"

e2e_init_artifacts
e2e_banner "Startup Banner JSON E2E Test Suite"
e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in script timeout python3; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

pick_port() {
python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

normalize_transcript() {
    local in_path="$1"
    local out_path="$2"
    python3 - <<'PY' "${in_path}" "${out_path}"
import re
import sys

data = open(sys.argv[1], "rb").read()
data = re.sub(rb"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)", b"", data)
data = re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", data)
data = re.sub(rb"\x1b[@-_]", b"", data)
text = data.decode("utf-8", errors="replace")
lines = [
    line for line in text.splitlines()
    if not line.startswith("Script started on ") and not line.startswith("Script done on ")
]
open(sys.argv[2], "w", encoding="utf-8").write("\n".join(lines) + "\n")
PY
}

start_server_pty() {
    local label="$1"
    local port="$2"
    local db_path="$3"
    local storage_root="$4"
    local bin="$5"
    shift 5
    local -a env_overrides=("$@")
    local transcript="${E2E_ARTIFACT_DIR}/${label}.typescript"
    local timeout_s="${AM_E2E_SERVER_TIMEOUT_S:-10}"
    local -a cmd_parts=(
        env
        "DATABASE_URL=sqlite:////${db_path}"
        "STORAGE_ROOT=${storage_root}"
        "HTTP_HOST=127.0.0.1"
        "HTTP_PORT=${port}"
        "HTTP_RBAC_ENABLED=0"
        "HTTP_RATE_LIMIT_ENABLED=0"
        "HTTP_JWT_ENABLED=0"
        "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0"
        "AM_ATC_ENABLED=false"
    )
    local override
    for override in "${env_overrides[@]}"; do
        cmd_parts+=("${override}")
    done
    cmd_parts+=(timeout "${timeout_s}s" "${bin}" serve-http --host 127.0.0.1 --port "${port}")
    local server_cmd=""
    local part
    for part in "${cmd_parts[@]}"; do
        printf -v server_cmd '%s %q' "${server_cmd}" "${part}"
    done
    server_cmd="${server_cmd# }"
    printf '%s\n' "${server_cmd}" > "${E2E_ARTIFACT_DIR}/${label}.command.txt"

    (script -q -f -c "${server_cmd}" "${transcript}") >/dev/null 2>&1 &
    echo "$!"
}

stop_server_pty() {
    local pid="$1"
    if kill -0 "${pid}" 2>/dev/null; then
        kill "${pid}" 2>/dev/null || true
        sleep 0.2
        kill -9 "${pid}" 2>/dev/null || true
    fi
}

extract_startup_json() {
    local input="$1"
    local output="$2"
    python3 - <<'PY' "${input}" "${output}"
import json
import sys

text = open(sys.argv[1], encoding="utf-8").read()
begin = "--- BEGIN STARTUP STATE (JSON) ---"
end = "--- END STARTUP STATE (JSON) ---"
start = text.find(begin)
if start < 0:
    raise SystemExit("missing begin marker")
start += len(begin)
finish = text.find(end, start)
if finish < 0:
    raise SystemExit("missing end marker")
payload = text[start:finish].strip()
parsed = json.loads(payload)
open(sys.argv[2], "w", encoding="utf-8").write(json.dumps(parsed, indent=2, sort_keys=False))
PY
}

assert_json_field() {
    local label="$1"
    local json_path="$2"
    local expr="$3"
    set +e
    python3 - <<'PY' "${json_path}" "${expr}"
import json
import sys

doc = json.load(open(sys.argv[1], encoding="utf-8"))
if not eval(sys.argv[2], {"doc": doc}):
    raise SystemExit(1)
PY
    local rc=$?
    set -e
    if [ "${rc}" -eq 0 ]; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}"
    fi
}

BIN="$(e2e_ensure_binary "am" | tail -n 1)"

e2e_case_banner "startup JSON block is parseable and masked"
WORK1="$(e2e_mktemp "e2e_banner_json")"
DB1="${WORK1}/db.sqlite3"
STORAGE1="${WORK1}/storage"
mkdir -p "${STORAGE1}"
PORT1="$(pick_port)"
PID1="$(start_server_pty "json_enabled" "${PORT1}" "${DB1}" "${STORAGE1}" "${BIN}" \
    "HTTP_BEARER_TOKEN=secret123" "AM_GIT_BINARY=/usr/local/bin/git-2.50.x")"
if ! e2e_wait_port 127.0.0.1 "${PORT1}" 10; then
    stop_server_pty "${PID1}"
    e2e_fail "server starts for JSON banner case"
    e2e_summary
    exit 1
fi
sleep 0.6
stop_server_pty "${PID1}"

NORM1="${E2E_ARTIFACT_DIR}/json_enabled.normalized.txt"
JSON1="${E2E_ARTIFACT_DIR}/json_enabled.startup.json"
normalize_transcript "${E2E_ARTIFACT_DIR}/json_enabled.typescript" "${NORM1}"
if extract_startup_json "${NORM1}" "${JSON1}"; then
    e2e_pass "startup JSON markers contain parseable JSON"
else
    e2e_fail "startup JSON markers contain parseable JSON"
fi
assert_json_field "JSON includes endpoint" "${JSON1}" "doc['endpoint'].startswith('http://127.0.0.1:')"
assert_json_field "JSON includes stats object" "${JSON1}" "all(k in doc['stats'] for k in ['projects','agents','messages','file_reservations','contact_links'])"
assert_json_field "bearer token is masked" "${JSON1}" "doc['runtime']['http_bearer_token'] == '<redacted>'"
assert_json_field "AM_GIT_BINARY path is reduced to basename" "${JSON1}" "doc['runtime']['am_git_binary'] == 'git-2.50.x'"

e2e_case_banner "AM_BANNER_JSON_DISABLED skips startup JSON block"
WORK2="$(e2e_mktemp "e2e_banner_json_disabled")"
DB2="${WORK2}/db.sqlite3"
STORAGE2="${WORK2}/storage"
mkdir -p "${STORAGE2}"
PORT2="$(pick_port)"
PID2="$(start_server_pty "json_disabled" "${PORT2}" "${DB2}" "${STORAGE2}" "${BIN}" \
    "AM_BANNER_JSON_DISABLED=1")"
if ! e2e_wait_port 127.0.0.1 "${PORT2}" 10; then
    stop_server_pty "${PID2}"
    e2e_fail "server starts for disabled JSON banner case"
    e2e_summary
    exit 1
fi
sleep 0.6
stop_server_pty "${PID2}"

NORM2="${E2E_ARTIFACT_DIR}/json_disabled.normalized.txt"
normalize_transcript "${E2E_ARTIFACT_DIR}/json_disabled.typescript" "${NORM2}"
if grep -Fq -- "--- BEGIN STARTUP STATE (JSON) ---" "${NORM2}"; then
    e2e_fail "disabled env skips startup JSON markers"
else
    e2e_pass "disabled env skips startup JSON markers"
fi

e2e_summary
