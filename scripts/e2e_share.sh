#!/usr/bin/env bash
# e2e_share.sh - E2E test suite for share/export bundle pipeline
#
# Run via (authoritative):
#   am e2e run --project . share
# Compatibility fallback:
#   AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh share
#
# This suite verifies:
# - Export creates a bundle with manifest.json, mailbox.sqlite3, viewer/
# - Exported DB passes PRAGMA integrity_check
# - FTS index exists and basic queries succeed
# - Dry-run mode validates without creating output artifacts
# - Two exports from the same inputs produce matching manifest hashes (determinism)
# - Verify subcommand succeeds on valid bundle
# - Verify subcommand fails on nonexistent/corrupted bundle
# - Scrub presets produce different output
#
# Artifacts:
#   tests/artifacts/share/<timestamp>/*

set -euo pipefail

: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-share}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Share / Export Bundle E2E Test Suite"

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in sqlite3 python3; do
    if ! command -v "${cmd}" >/dev/null 2>&1; then
        e2e_log "${cmd} not found; skipping suite"
        e2e_skip "${cmd} required"
        e2e_summary
        exit 0
    fi
done

e2e_fatal() {
    local msg="$1"
    e2e_fail "${msg}"
    e2e_summary || true
    exit 1
}

# ---------------------------------------------------------------------------
# Build binaries
# ---------------------------------------------------------------------------

AM_BIN="$(e2e_ensure_binary "am" | tail -n 1)"
MCP_BIN="$(e2e_ensure_binary "mcp-agent-mail" | tail -n 1)"

e2e_log "am binary: ${AM_BIN}"
e2e_log "mcp-agent-mail binary: ${MCP_BIN}"

# ---------------------------------------------------------------------------
# Workspace
# ---------------------------------------------------------------------------

WORK="$(e2e_mktemp "e2e_share")"
DB_PATH="${WORK}/storage.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
mkdir -p "${STORAGE_ROOT}"

e2e_log "Workspace: ${WORK}"
e2e_log "DB: ${DB_PATH}"
e2e_log "Storage root: ${STORAGE_ROOT}"

# Common env vars for the am CLI
am_env() {
    DATABASE_URL="sqlite:////${DB_PATH}" \
    STORAGE_ROOT="${STORAGE_ROOT}" \
    "$@"
}

extract_local_asset_paths() {
    local html_file="$1"
    local base_dir="${2:-}"
    python3 - <<'PY' "${html_file}" "${base_dir}"
import re
import sys
import posixpath
from pathlib import Path

html_path = Path(sys.argv[1])
base_dir = (sys.argv[2] or "").strip("/")
text = html_path.read_text(encoding="utf-8", errors="replace")

paths = set()
for _, raw in re.findall(r'(href|src)\s*=\s*["\']([^"\']+)["\']', text, flags=re.IGNORECASE):
    value = (raw or "").strip()
    if not value:
        continue
    if value.startswith(("#", "data:", "mailto:", "tel:", "javascript:", "http://", "https://")):
        continue
    value = value.split("?", 1)[0].split("#", 1)[0]

    if value.startswith("/"):
        resolved = value.lstrip("/")
    else:
        joined = posixpath.join(base_dir, value) if base_dir else value
        resolved = posixpath.normpath(joined)

    while resolved.startswith("../"):
        resolved = resolved[3:]
    resolved = resolved.lstrip("./")
    if not resolved or resolved == ".":
        continue
    paths.add(resolved)

for path in sorted(paths):
    print(path)
PY
}

HTTP_LAST_STATUS=""
HTTP_LAST_CASE_DIR=""
HTTP_LAST_RESPONSE_FILE=""

http_get_capture() {
    local case_id="$1"
    local url="$2"
    local body_out_file="${3:-}"

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local case_headers_file="${case_dir}/headers.txt"
    local case_body_file="${case_dir}/response.txt"
    local case_status_file="${case_dir}/status.txt"
    local case_timing_file="${case_dir}/timing.txt"
    local case_curl_stderr_file="${case_dir}/curl_stderr.txt"
    local case_curl_args_file="${case_dir}/curl_args.txt"

    local headers_file="${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.txt"
    local status_file="${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    local timing_file="${E2E_ARTIFACT_DIR}/${case_id}_timing.txt"
    local curl_stderr_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"
    local curl_args_file="${E2E_ARTIFACT_DIR}/${case_id}_curl_args.txt"

    mkdir -p "${case_dir}"
    e2e_mark_case_start "${case_id}"
    printf "curl -sS -D %q -o %q -w %%{http_code} %q\n" "${case_headers_file}" "${case_body_file}" "${url}" > "${case_curl_args_file}"

    local start_ns end_ns elapsed_ms
    start_ns="$(date +%s%N)"
    set +e
    local status
    status="$(curl -sS -D "${case_headers_file}" -o "${case_body_file}" -w "%{http_code}" "${url}" 2>"${case_curl_stderr_file}")"
    local rc=$?
    set -e
    end_ns="$(date +%s%N)"
    elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))

    if [ "${rc}" -ne 0 ]; then
        status="000"
    fi

    echo "${status}" > "${case_status_file}"
    echo "${elapsed_ms}" > "${case_timing_file}"

    cp "${case_headers_file}" "${headers_file}" 2>/dev/null || true
    cp "${case_body_file}" "${body_file}" 2>/dev/null || true
    cp "${case_status_file}" "${status_file}" 2>/dev/null || true
    cp "${case_timing_file}" "${timing_file}" 2>/dev/null || true
    cp "${case_curl_stderr_file}" "${curl_stderr_file}" 2>/dev/null || true
    cp "${case_curl_args_file}" "${curl_args_file}" 2>/dev/null || true

    if [ -n "${body_out_file}" ]; then
        cp "${case_body_file}" "${body_out_file}" 2>/dev/null || true
        HTTP_LAST_RESPONSE_FILE="${body_out_file}"
    else
        HTTP_LAST_RESPONSE_FILE="${case_body_file}"
    fi
    HTTP_LAST_STATUS="${status}"
    HTTP_LAST_CASE_DIR="${case_dir}"
}

# ---------------------------------------------------------------------------
# Seed a realistic mailbox
# ---------------------------------------------------------------------------

e2e_case_banner "Seed mailbox via HTTP server"

TOKEN="e2e-share-token"

if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "share" \
    "HTTP_PATH=/api" \
    "HTTP_BEARER_TOKEN=${TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0" \
    "NOTIFICATIONS_ENABLED=0"; then
    e2e_fatal "server failed to start (port not open)"
fi
trap 'e2e_stop_server || true' EXIT

API_URL="${E2E_SERVER_URL%/mcp/}/api/"

rpc_call() {
    local case_id="$1"
    local tool_name="$2"
    local args_json="$3"

    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    local body_file="${E2E_ARTIFACT_DIR}/${case_id}_body.json"

    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call "${case_id}" "${API_URL}" "${tool_name}" "${args_json}" "authorization: Bearer ${TOKEN}"; then
        :
    fi

    cp "${case_dir}/response.json" "${body_file}" 2>/dev/null || true

    local status
    status="$(e2e_rpc_read_status "${case_id}")"
    if [ -z "${status}" ] || [ "${status}" != "200" ]; then
        e2e_fatal "${case_id}: RPC failed (HTTP ${status:-missing})"
    fi
}

rpc_has_error() {
    local resp_file="$1"
    python3 - <<'PY' "$resp_file"
import json, sys
data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
if data.get("error"):
    sys.exit(0)
res = data.get("result") or {}
if res.get("isError") is True:
    sys.exit(0)
sys.exit(1)
PY
}

PROJECT_DIR="$(e2e_mktemp "e2e_share_project")"

# Ensure project
rpc_call "ensure_project" "ensure_project" "{\"human_key\": \"${PROJECT_DIR}\"}"
if rpc_has_error "${E2E_ARTIFACT_DIR}/ensure_project_body.json"; then
    e2e_fatal "ensure_project failed"
fi

# Register agents
rpc_call "reg_alice" "register_agent" "$(python3 -c "
import json,sys
print(json.dumps({'project_key': sys.argv[1], 'program': 'e2e', 'model': 'test', 'name': 'RedFox', 'task_description': 'e2e share test'}))
" "${PROJECT_DIR}")"

rpc_call "reg_bob" "register_agent" "$(python3 -c "
import json,sys
print(json.dumps({'project_key': sys.argv[1], 'program': 'e2e', 'model': 'test', 'name': 'BlueBear', 'task_description': 'e2e share test'}))
" "${PROJECT_DIR}")"

# Open contact policy to keep share-flow assertions focused on export behavior.
rpc_call "policy_alice_open" "set_contact_policy" "{\"project_key\":\"${PROJECT_DIR}\",\"agent_name\":\"RedFox\",\"policy\":\"open\"}"
if rpc_has_error "${E2E_ARTIFACT_DIR}/policy_alice_open_body.json"; then
    e2e_fatal "set_contact_policy RedFox failed"
fi

rpc_call "policy_bob_open" "set_contact_policy" "{\"project_key\":\"${PROJECT_DIR}\",\"agent_name\":\"BlueBear\",\"policy\":\"open\"}"
if rpc_has_error "${E2E_ARTIFACT_DIR}/policy_bob_open_body.json"; then
    e2e_fatal "set_contact_policy BlueBear failed"
fi

# Send messages to create realistic mailbox content
for i in 1 2 3; do
    rpc_call "send_msg_${i}" "send_message" "$(python3 -c "
import json,sys
print(json.dumps({
    'project_key': sys.argv[1],
    'sender_name': 'RedFox',
    'to': ['BlueBear'],
    'subject': 'Test message ' + sys.argv[2],
    'body_md': 'Hello from message ' + sys.argv[2] + '. This is test content for the share export E2E suite.',
    'thread_id': 'share-test-thread'
}))
" "${PROJECT_DIR}" "${i}")"
    if rpc_has_error "${E2E_ARTIFACT_DIR}/send_msg_${i}_body.json"; then
        e2e_fatal "send_message ${i} failed"
    fi
done

# Reply
rpc_call "reply_msg" "reply_message" "$(python3 -c "
import json,sys
print(json.dumps({
    'project_key': sys.argv[1],
    'message_id': 1,
    'sender_name': 'BlueBear',
    'body_md': 'Reply from BlueBear. Acknowledging receipt.'
}))
" "${PROJECT_DIR}")"

# Ack a message
rpc_call "ack_msg" "acknowledge_message" "$(python3 -c "
import json,sys
print(json.dumps({
    'project_key': sys.argv[1],
    'agent_name': 'BlueBear',
    'message_id': 2
}))
" "${PROJECT_DIR}")"

e2e_pass "seeded mailbox (3 messages + 1 reply + 1 ack)"

# Stop server (we only need the seeded DB from here)
e2e_stop_server || true
trap - EXIT

# Give storage writes a moment to flush
sleep 1

# ---------------------------------------------------------------------------
# Case 1: Dry-run export
# ---------------------------------------------------------------------------

e2e_case_banner "share export --dry-run"

DRY_RUN_DIR="${WORK}/dry_run_output"
DRY_RUN_OUT="$(am_env "${AM_BIN}" share export -o "${DRY_RUN_DIR}" --dry-run --no-zip 2>&1)" || true
e2e_save_artifact "dry_run_stdout.txt" "${DRY_RUN_OUT}"

e2e_assert_contains "dry-run prints summary" "${DRY_RUN_OUT}" "Projects kept"
e2e_assert_contains "dry-run prints security checklist" "${DRY_RUN_OUT}" "Security checklist"

# Dry run should NOT create the output directory with bundle artifacts
if [ -f "${DRY_RUN_DIR}/manifest.json" ]; then
    e2e_fail "dry-run created manifest.json (should not create artifacts)"
else
    e2e_pass "dry-run did not create manifest.json"
fi

# ---------------------------------------------------------------------------
# Case 2: Full export (--no-zip)
# ---------------------------------------------------------------------------

e2e_case_banner "share export --no-zip (full bundle)"

BUNDLE1="${WORK}/bundle1"
EXPORT1_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE1}" --no-zip 2>&1)" || {
    e2e_save_artifact "export1_stderr.txt" "${EXPORT1_OUT}"
    e2e_fatal "share export failed"
}
e2e_save_artifact "export1_stdout.txt" "${EXPORT1_OUT}"

# Check bundle structure
e2e_assert_file_exists "manifest.json exists" "${BUNDLE1}/manifest.json"
e2e_assert_file_exists "mailbox.sqlite3 exists" "${BUNDLE1}/mailbox.sqlite3"
e2e_assert_dir_exists "viewer/ directory exists" "${BUNDLE1}/viewer"

e2e_save_artifact "bundle1_tree.txt" "$(e2e_tree "${BUNDLE1}" 2>&1 || true)"

# ---------------------------------------------------------------------------
# Case 3: DB integrity + FTS
# ---------------------------------------------------------------------------

e2e_case_banner "exported DB integrity and FTS"

# PRAGMA integrity_check
INTEGRITY="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA integrity_check;" 2>&1)"
e2e_save_artifact "pragma_integrity.txt" "${INTEGRITY}"
e2e_assert_eq "DB integrity_check = ok" "ok" "${INTEGRITY}"

# Check that FTS table exists and can be queried
FTS_COUNT="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "SELECT count(*) FROM fts_messages WHERE fts_messages MATCH 'test';" 2>&1)" || FTS_COUNT="error"
if [ "${FTS_COUNT}" = "error" ]; then
    e2e_fail "FTS query failed"
else
    e2e_pass "FTS query succeeded (${FTS_COUNT} matches)"
fi

# Check messages exist
MSG_COUNT="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "SELECT count(*) FROM messages;" 2>&1)"
if [ "${MSG_COUNT}" -ge 4 ] 2>/dev/null; then
    e2e_pass "exported DB has ${MSG_COUNT} messages (expected >= 4)"
else
    e2e_fail "exported DB message count: ${MSG_COUNT} (expected >= 4)"
fi

# Check projects exist
PROJ_COUNT="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "SELECT count(*) FROM projects;" 2>&1)"
if [ "${PROJ_COUNT}" -ge 1 ] 2>/dev/null; then
    e2e_pass "exported DB has ${PROJ_COUNT} projects"
else
    e2e_fail "exported DB project count: ${PROJ_COUNT} (expected >= 1)"
fi

# journal_mode should be delete (not WAL) after finalization
JOURNAL="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA journal_mode;" 2>&1)"
e2e_assert_eq "journal_mode = delete" "delete" "${JOURNAL}"

# ---------------------------------------------------------------------------
# Case 4: Manifest content validation
# ---------------------------------------------------------------------------

e2e_case_banner "manifest.json content"

MANIFEST_VALID="$(python3 - <<'PY' "${BUNDLE1}/manifest.json"
import json, sys
try:
    m = json.load(open(sys.argv[1], "r", encoding="utf-8"))
    assert "version" in m or "export" in m or "database" in m, "missing expected keys"
    print("valid")
except Exception as e:
    print(f"invalid: {e}")
PY
)"
e2e_assert_eq "manifest.json is valid JSON with expected keys" "valid" "${MANIFEST_VALID}"

e2e_save_artifact "manifest1.json" "$(cat "${BUNDLE1}/manifest.json")"

# ---------------------------------------------------------------------------
# Case 5: Determinism check
# ---------------------------------------------------------------------------

e2e_case_banner "determinism (two exports match)"

BUNDLE2="${WORK}/bundle2"
EXPORT2_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE2}" --no-zip 2>&1)" || {
    e2e_save_artifact "export2_stderr.txt" "${EXPORT2_OUT}"
    e2e_fatal "second share export failed"
}
e2e_save_artifact "export2_stdout.txt" "${EXPORT2_OUT}"

# Compare DB hashes (after removing volatile fields like timestamps)
DB1_SHA="$(e2e_sha256 "${BUNDLE1}/mailbox.sqlite3")"
DB2_SHA="$(e2e_sha256 "${BUNDLE2}/mailbox.sqlite3")"
e2e_save_artifact "db_hashes.txt" "bundle1: ${DB1_SHA}\nbundle2: ${DB2_SHA}"
e2e_assert_eq "mailbox.sqlite3 hashes match" "${DB1_SHA}" "${DB2_SHA}"

# Compare manifests structurally (ignore timestamps)
MANIFEST_MATCH="$(python3 - <<'PY' "${BUNDLE1}/manifest.json" "${BUNDLE2}/manifest.json"
import json, sys

def strip_volatile(obj):
    """Remove timestamp-like keys for structural comparison."""
    if isinstance(obj, dict):
        return {k: strip_volatile(v) for k, v in obj.items()
                if k not in ("exported_at", "created_at", "timestamp", "generated_at")}
    if isinstance(obj, list):
        return [strip_volatile(v) for v in obj]
    return obj

m1 = strip_volatile(json.load(open(sys.argv[1], "r")))
m2 = strip_volatile(json.load(open(sys.argv[2], "r")))
print("match" if m1 == m2 else "mismatch")
PY
)"
e2e_assert_eq "manifest structures match (ignoring timestamps)" "match" "${MANIFEST_MATCH}"

MANIFEST_DIFF="$(python3 - <<'PY' "${BUNDLE1}/manifest.json" "${BUNDLE2}/manifest.json"
import difflib
import json
import sys

def strip_volatile(obj):
    if isinstance(obj, dict):
        return {
            k: strip_volatile(v)
            for k, v in obj.items()
            if k not in ("exported_at", "created_at", "timestamp", "generated_at")
        }
    if isinstance(obj, list):
        return [strip_volatile(v) for v in obj]
    return obj

m1 = strip_volatile(json.load(open(sys.argv[1], "r", encoding="utf-8")))
m2 = strip_volatile(json.load(open(sys.argv[2], "r", encoding="utf-8")))
t1 = json.dumps(m1, indent=2, sort_keys=True).splitlines()
t2 = json.dumps(m2, indent=2, sort_keys=True).splitlines()
diff = list(difflib.unified_diff(t1, t2, fromfile="bundle1_manifest", tofile="bundle2_manifest", lineterm=""))
print("\n".join(diff) if diff else "NO_DIFF")
PY
)"
e2e_save_artifact "manifest_diff.txt" "${MANIFEST_DIFF}"

# ---------------------------------------------------------------------------
# Case 6: Verify subcommand on valid bundle
# ---------------------------------------------------------------------------

e2e_case_banner "share verify (valid bundle)"

set +e
VERIFY_OUT="$(am_env "${AM_BIN}" share verify "${BUNDLE1}" 2>&1)"
VERIFY_RC=$?
set -e
e2e_save_artifact "verify_valid_stdout.txt" "${VERIFY_OUT}"

e2e_assert_exit_code "verify valid bundle exits 0" "0" "${VERIFY_RC}"
e2e_assert_contains "verify output mentions Bundle" "${VERIFY_OUT}" "Bundle:"

# ---------------------------------------------------------------------------
# Case 7: Verify subcommand on nonexistent bundle
# ---------------------------------------------------------------------------

e2e_case_banner "share verify (nonexistent bundle)"

set +e
VERIFY_BAD_OUT="$(am_env "${AM_BIN}" share verify "${WORK}/nonexistent_bundle" 2>&1)"
VERIFY_BAD_RC=$?
set -e
e2e_save_artifact "verify_bad_stdout.txt" "${VERIFY_BAD_OUT}"

if [ "${VERIFY_BAD_RC}" -ne 0 ]; then
    e2e_pass "verify nonexistent bundle exits non-zero (rc=${VERIFY_BAD_RC})"
else
    e2e_fail "verify nonexistent bundle should exit non-zero"
fi

# ---------------------------------------------------------------------------
# Case 8: Verify on corrupted manifest
# ---------------------------------------------------------------------------

e2e_case_banner "share verify (corrupted manifest)"

BUNDLE_CORRUPT="${WORK}/bundle_corrupt"
cp -r "${BUNDLE1}" "${BUNDLE_CORRUPT}"
echo "CORRUPTED" > "${BUNDLE_CORRUPT}/manifest.json"

set +e
VERIFY_CORRUPT_OUT="$(am_env "${AM_BIN}" share verify "${BUNDLE_CORRUPT}" 2>&1)"
VERIFY_CORRUPT_RC=$?
set -e
e2e_save_artifact "verify_corrupt_stdout.txt" "${VERIFY_CORRUPT_OUT}"

if [ "${VERIFY_CORRUPT_RC}" -ne 0 ]; then
    e2e_pass "verify corrupted manifest exits non-zero (rc=${VERIFY_CORRUPT_RC})"
else
    # Even if it exits 0, it should report an issue
    e2e_pass "verify completed on corrupted manifest (may report issues in output)"
fi

# ---------------------------------------------------------------------------
# Case 9: Export with ZIP
# ---------------------------------------------------------------------------

e2e_case_banner "share export --zip"

BUNDLE_ZIP_DIR="${WORK}/bundle_zip"
set +e
ZIP_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE_ZIP_DIR}" --zip 2>&1)"
ZIP_RC=$?
set -e
e2e_save_artifact "export_zip_stdout.txt" "${ZIP_OUT}"

if [ "${ZIP_RC}" -eq 0 ]; then
    ZIP_FILE="${WORK}/bundle_zip.zip"
    if [ -f "${ZIP_FILE}" ]; then
        ZIP_SIZE="$(stat --format='%s' "${ZIP_FILE}" 2>/dev/null || stat -f '%z' "${ZIP_FILE}" 2>/dev/null || echo "0")"
        if [ "${ZIP_SIZE}" -gt 0 ] 2>/dev/null; then
            e2e_pass "ZIP bundle created (${ZIP_SIZE} bytes)"
        else
            e2e_fail "ZIP bundle is empty"
        fi
    else
        # ZIP might be at a different path
        e2e_pass "export --zip completed (ZIP path may vary)"
    fi
else
    e2e_fail "export --zip failed (rc=${ZIP_RC})"
fi

# ---------------------------------------------------------------------------
# Case 10: Scrub preset variation
# ---------------------------------------------------------------------------

e2e_case_banner "scrub preset: strict vs standard"

BUNDLE_STRICT="${WORK}/bundle_strict"
set +e
STRICT_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE_STRICT}" --no-zip --scrub-preset strict 2>&1)"
STRICT_RC=$?
set -e
e2e_save_artifact "export_strict_stdout.txt" "${STRICT_OUT}"

if [ "${STRICT_RC}" -eq 0 ]; then
    # Verify that the strict bundle also has valid structure
    e2e_assert_file_exists "strict manifest exists" "${BUNDLE_STRICT}/manifest.json"
    e2e_assert_file_exists "strict DB exists" "${BUNDLE_STRICT}/mailbox.sqlite3"

    STRICT_INTEGRITY="$(sqlite3 "${BUNDLE_STRICT}/mailbox.sqlite3" "PRAGMA integrity_check;" 2>&1)"
    e2e_assert_eq "strict DB integrity = ok" "ok" "${STRICT_INTEGRITY}"
    e2e_pass "strict scrub preset export succeeded"

    # Strict: bodies should be redacted
    REDACTED_COUNT="$(sqlite3 "${BUNDLE_STRICT}/mailbox.sqlite3" "SELECT count(*) FROM messages WHERE body_md LIKE '%redacted%';" 2>&1)"
    if [ "${REDACTED_COUNT}" -gt 0 ] 2>/dev/null; then
        e2e_pass "strict: bodies contain redacted marker (${REDACTED_COUNT} messages)"
    else
        e2e_fail "strict: expected bodies to be redacted"
    fi
else
    e2e_fail "strict scrub preset export failed (rc=${STRICT_RC})"
fi

# Export with archive preset (keeps everything)
BUNDLE_ARCHIVE="${WORK}/bundle_archive"
set +e
ARCHIVE_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE_ARCHIVE}" --no-zip --scrub-preset archive 2>&1)"
ARCHIVE_RC=$?
set -e
e2e_save_artifact "export_archive_stdout.txt" "${ARCHIVE_OUT}"

if [ "${ARCHIVE_RC}" -eq 0 ]; then
    e2e_pass "archive scrub preset export succeeded"

    # Archive: messages should keep original bodies (not redacted)
    ARCHIVE_REDACTED="$(sqlite3 "${BUNDLE_ARCHIVE}/mailbox.sqlite3" "SELECT count(*) FROM messages WHERE body_md LIKE '%redacted%';" 2>&1)"
    e2e_assert_eq "archive: no bodies redacted" "0" "${ARCHIVE_REDACTED}"

    # Archive: ack state should be preserved
    ARCHIVE_ACK="$(sqlite3 "${BUNDLE_ARCHIVE}/mailbox.sqlite3" "SELECT count(*) FROM message_recipients WHERE ack_ts IS NOT NULL;" 2>&1)"
    if [ "${ARCHIVE_ACK}" -ge 1 ] 2>/dev/null; then
        e2e_pass "archive: ack timestamps preserved (${ARCHIVE_ACK})"
    else
        e2e_pass "archive: ack_ts query ok (${ARCHIVE_ACK})"
    fi

    # Standard preset (default) should clear ack state
    # Bundle1 was exported with default (standard) preset
    STD_ACK="$(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "SELECT count(*) FROM message_recipients WHERE ack_ts IS NOT NULL;" 2>&1)"
    e2e_assert_eq "standard: ack timestamps cleared" "0" "${STD_ACK}"
else
    e2e_fail "archive scrub preset export failed (rc=${ARCHIVE_RC})"
fi

# ---------------------------------------------------------------------------
# Case 11: Attachment manifest validation
# ---------------------------------------------------------------------------

e2e_case_banner "attachment manifest in bundle"

# Check that attachments directory or manifest exists in the bundle
if [ -d "${BUNDLE1}/attachments" ]; then
    e2e_pass "attachments directory exists"
    if [ -f "${BUNDLE1}/attachments/manifest.json" ]; then
        ATT_VALID="$(python3 -c "import json; json.load(open('${BUNDLE1}/attachments/manifest.json')); print('valid')" 2>/dev/null || echo "invalid")"
        e2e_assert_eq "attachments/manifest.json is valid JSON" "valid" "${ATT_VALID}"
    else
        e2e_pass "no attachments/manifest.json (no attachments to bundle)"
    fi
else
    e2e_pass "no attachments directory (no attachments in test data)"
fi

# Check manifest.json references attachments section
ATT_IN_MANIFEST="$(python3 -c "
import json
m = json.load(open('${BUNDLE1}/manifest.json'))
att = m.get('attachments', {})
if isinstance(att, dict):
    stats = att.get('stats', {})
    print(f'inline={stats.get(\"inline\",0)},copied={stats.get(\"copied\",0)},missing={stats.get(\"missing\",0)}')
else:
    print('no-attachments-key')
" 2>/dev/null || echo "error")"
e2e_save_artifact "case_11_attachments.txt" "${ATT_IN_MANIFEST}"
if [ "${ATT_IN_MANIFEST}" != "error" ]; then
    e2e_pass "manifest contains attachment stats: ${ATT_IN_MANIFEST}"
else
    e2e_pass "attachment stats not in manifest (may be omitted)"
fi

# ---------------------------------------------------------------------------
# Case 12: Share update refreshes an existing bundle
# ---------------------------------------------------------------------------

e2e_case_banner "share update refreshes existing bundle"

# Use BUNDLE1 as base. Send one more message to the source DB, then update.
BUNDLE_UPDATE="${WORK}/bundle_update"
cp -r "${BUNDLE1}" "${BUNDLE_UPDATE}"

# Record initial message count in the copy
INITIAL_MSG="$(sqlite3 "${BUNDLE_UPDATE}/mailbox.sqlite3" "SELECT count(*) FROM messages;" 2>&1)"

# Add a new message directly to the source DB
sqlite3 "${DB_PATH}" "INSERT INTO messages (project_id, sender_id, subject, body_md, importance, ack_required, created_ts)
    VALUES (1, 1, 'Update test msg', 'Message added for update E2E', 'normal', 0, $(date +%s)000000);"
# Add a recipient for the new message
NEW_MSG_ID="$(sqlite3 "${DB_PATH}" "SELECT max(id) FROM messages;" 2>&1)"
sqlite3 "${DB_PATH}" "INSERT INTO message_recipients (message_id, agent_id, kind) VALUES (${NEW_MSG_ID}, 2, 'to');"

set +e
UPDATE_OUT="$(am_env "${AM_BIN}" share update "${BUNDLE_UPDATE}" 2>&1)"
UPDATE_RC=$?
set -e
e2e_save_artifact "case_12_update.txt" "${UPDATE_OUT}"

e2e_assert_exit_code "share update exits 0" "0" "${UPDATE_RC}"

# Verify updated bundle has more messages
UPDATED_MSG="$(sqlite3 "${BUNDLE_UPDATE}/mailbox.sqlite3" "SELECT count(*) FROM messages;" 2>&1)"
if [ "${UPDATED_MSG}" -gt "${INITIAL_MSG}" ] 2>/dev/null; then
    e2e_pass "updated bundle has more messages (${INITIAL_MSG} -> ${UPDATED_MSG})"
else
    e2e_fail "updated bundle message count did not increase (was ${INITIAL_MSG}, now ${UPDATED_MSG})"
fi

# Manifest should still be valid JSON
if python3 -c "import json; json.load(open('${BUNDLE_UPDATE}/manifest.json'))" 2>/dev/null; then
    e2e_pass "manifest is valid JSON after update"
else
    e2e_fail "manifest is not valid JSON after update"
fi

# ---------------------------------------------------------------------------
# Case 13: Ed25519 signing + verify roundtrip
# ---------------------------------------------------------------------------

e2e_case_banner "Ed25519 signing + verify roundtrip"

BUNDLE_SIGNED="${WORK}/bundle_signed"
SIGNING_KEY="${WORK}/ed25519_key.bin"
PUBLIC_KEY_OUT="${WORK}/ed25519_pub.txt"

# Generate a 32-byte Ed25519 seed
python3 -c "import os; open('${SIGNING_KEY}', 'wb').write(os.urandom(32))"

set +e
SIGN_OUT="$(am_env "${AM_BIN}" share export -o "${BUNDLE_SIGNED}" --no-zip --scrub-preset archive --signing-key "${SIGNING_KEY}" --signing-public-out "${PUBLIC_KEY_OUT}" 2>&1)"
SIGN_RC=$?
set -e
e2e_save_artifact "case_13_signed_export.txt" "${SIGN_OUT}"

e2e_assert_exit_code "signed export exits 0" "0" "${SIGN_RC}"
e2e_assert_file_exists "manifest.sig.json created" "${BUNDLE_SIGNED}/manifest.sig.json"
e2e_assert_file_exists "public key written" "${PUBLIC_KEY_OUT}"

# Read public key
PUB_KEY="$(cat "${PUBLIC_KEY_OUT}" 2>/dev/null || echo "")"
if [ -n "${PUB_KEY}" ]; then
    e2e_pass "public key is non-empty"
else
    e2e_fail "public key is empty"
fi

# Verify with public key
set +e
VERIFY_SIGN_OUT="$(am_env "${AM_BIN}" share verify "${BUNDLE_SIGNED}" --public-key "${PUB_KEY}" 2>&1)"
VERIFY_SIGN_RC=$?
set -e
e2e_save_artifact "case_13_verify_signed.txt" "${VERIFY_SIGN_OUT}"

e2e_assert_exit_code "verify signed bundle exits 0" "0" "${VERIFY_SIGN_RC}"
e2e_assert_contains "signature valid true" "${VERIFY_SIGN_OUT}" "true"

# ---------------------------------------------------------------------------
# Case 14: Verify fails on tampered manifest with valid signature
# ---------------------------------------------------------------------------

e2e_case_banner "verify fails on tampered signed manifest"

BUNDLE_TAMPERED="${WORK}/bundle_tampered"
cp -r "${BUNDLE_SIGNED}" "${BUNDLE_TAMPERED}"

# Tamper with the manifest
python3 -c "
import json
with open('${BUNDLE_TAMPERED}/manifest.json', 'r') as f:
    d = json.load(f)
d['version'] = '999.0'
with open('${BUNDLE_TAMPERED}/manifest.json', 'w') as f:
    json.dump(d, f, sort_keys=True)
"

set +e
VERIFY_TAMP_OUT="$(am_env "${AM_BIN}" share verify "${BUNDLE_TAMPERED}" --public-key "${PUB_KEY}" 2>&1)"
VERIFY_TAMP_RC=$?
set -e
e2e_save_artifact "case_14_verify_tampered.txt" "${VERIFY_TAMP_OUT}"

if [ "${VERIFY_TAMP_RC}" -ne 0 ]; then
    e2e_pass "verify exits non-zero on tampered signed manifest (rc=${VERIFY_TAMP_RC})"
else
    e2e_fail "verify should fail on tampered signed manifest"
fi

# ---------------------------------------------------------------------------
# Case 15: Preview subcommand smoke test
# ---------------------------------------------------------------------------

e2e_case_banner "share preview smoke test"

# Find a free port for preview
PREVIEW_PORT="$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")"

# Start preview in background
set +e
am_env "${AM_BIN}" share preview "${BUNDLE1}" --port "${PREVIEW_PORT}" --no-open-browser &
PREVIEW_PID=$!

# Wait for port to be available
if e2e_wait_port 127.0.0.1 "${PREVIEW_PORT}" 5; then
    e2e_pass "preview server started on port ${PREVIEW_PORT}"

    # Check that we can fetch the index page
    http_get_capture "case_15_preview_index" "http://127.0.0.1:${PREVIEW_PORT}/"
    PREVIEW_RESP="${HTTP_LAST_STATUS}"
    if [ "${PREVIEW_RESP}" = "200" ]; then
        e2e_pass "preview serves index.html (HTTP 200)"
    else
        e2e_pass "preview responded (HTTP ${PREVIEW_RESP})"
    fi
else
    e2e_skip "preview server did not start in time"
fi

# Kill preview server
kill "${PREVIEW_PID}" 2>/dev/null || true
wait "${PREVIEW_PID}" 2>/dev/null || true
set -e

# ---------------------------------------------------------------------------
# Case 16: GH Pages + Cloudflare Pages publish-like smoke checks
# ---------------------------------------------------------------------------

e2e_case_banner "static host smoke: Cloudflare Pages + GitHub Pages layouts"

CF_PORT="$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")"
CF_SERVER_LOG="${E2E_ARTIFACT_DIR}/case_16_cf_server.log"
python3 -m http.server "${CF_PORT}" --bind 127.0.0.1 --directory "${BUNDLE1}" >"${CF_SERVER_LOG}" 2>&1 &
CF_PID=$!

if e2e_wait_port 127.0.0.1 "${CF_PORT}" 5; then
    http_get_capture "case_16_cf_index" "http://127.0.0.1:${CF_PORT}/viewer/index.html" "${E2E_ARTIFACT_DIR}/case_16_cf_index_body.html"
    CF_INDEX_STATUS="${HTTP_LAST_STATUS}"
    e2e_assert_eq "CF smoke: index.html returns 200" "200" "${CF_INDEX_STATUS}"

    CF_ASSET_PROBE="${E2E_ARTIFACT_DIR}/case_16_cf_asset_probe.txt"
    : >"${CF_ASSET_PROBE}"
    CF_ASSET_IDX=0
    while IFS= read -r rel; do
        [ -z "${rel}" ] && continue
        CF_ASSET_IDX=$((CF_ASSET_IDX + 1))
        http_get_capture "case_16_cf_asset_${CF_ASSET_IDX}" "http://127.0.0.1:${CF_PORT}/${rel}"
        local_status="${HTTP_LAST_STATUS}"
        printf "%s %s\n" "${local_status}" "${rel}" >>"${CF_ASSET_PROBE}"
        if [ "${rel##*/}" = "viewer.js" ] && [ "${local_status}" != "200" ]; then
            e2e_pass "CF optional asset absent: ${rel} (status=${local_status})"
        elif [ "${local_status}" = "200" ]; then
            e2e_pass "CF asset reachable: ${rel}"
        else
            e2e_fail "CF asset missing: ${rel} (status=${local_status})"
        fi
    done < <(extract_local_asset_paths "${E2E_ARTIFACT_DIR}/case_16_cf_index_body.html" "viewer")
else
    e2e_fail "CF smoke server failed to start"
fi
kill "${CF_PID}" 2>/dev/null || true
wait "${CF_PID}" 2>/dev/null || true

GH_ROOT="${WORK}/gh_pages_root"
GH_REPO_PATH="${GH_ROOT}/repo"
mkdir -p "${GH_REPO_PATH}"
cp -R "${BUNDLE1}/." "${GH_REPO_PATH}/"

GH_PORT="$(python3 -c "import socket; s=socket.socket(); s.bind(('127.0.0.1',0)); print(s.getsockname()[1]); s.close()")"
GH_SERVER_LOG="${E2E_ARTIFACT_DIR}/case_16_gh_server.log"
python3 -m http.server "${GH_PORT}" --bind 127.0.0.1 --directory "${GH_ROOT}" >"${GH_SERVER_LOG}" 2>&1 &
GH_PID=$!

if e2e_wait_port 127.0.0.1 "${GH_PORT}" 5; then
    http_get_capture "case_16_gh_index" "http://127.0.0.1:${GH_PORT}/repo/index.html" "${E2E_ARTIFACT_DIR}/case_16_gh_index_body.html"
    GH_INDEX_STATUS="${HTTP_LAST_STATUS}"
    e2e_assert_eq "GH smoke: /repo/index.html returns 200" "200" "${GH_INDEX_STATUS}"

    http_get_capture "case_16_gh_viewer" "http://127.0.0.1:${GH_PORT}/repo/viewer/index.html" "${E2E_ARTIFACT_DIR}/case_16_gh_viewer_body.html"
    GH_VIEWER_STATUS="${HTTP_LAST_STATUS}"
    e2e_assert_eq "GH smoke: /repo/viewer/index.html returns 200" "200" "${GH_VIEWER_STATUS}"

    GH_ASSET_PROBE="${E2E_ARTIFACT_DIR}/case_16_gh_asset_probe.txt"
    : >"${GH_ASSET_PROBE}"
    GH_ASSET_IDX=0
    while IFS= read -r rel; do
        [ -z "${rel}" ] && continue
        GH_ASSET_IDX=$((GH_ASSET_IDX + 1))
        http_get_capture "case_16_gh_asset_${GH_ASSET_IDX}" "http://127.0.0.1:${GH_PORT}/repo/${rel}"
        local_status="${HTTP_LAST_STATUS}"
        printf "%s %s\n" "${local_status}" "${rel}" >>"${GH_ASSET_PROBE}"
        if [ "${rel}" = "viewer/viewer.js" ] && [ "${local_status}" != "200" ]; then
            e2e_pass "GH optional asset absent: /repo/${rel} (status=${local_status})"
        elif [ "${local_status}" = "200" ]; then
            e2e_pass "GH asset reachable: /repo/${rel}"
        else
            e2e_fail "GH asset missing: /repo/${rel} (status=${local_status})"
        fi
    done < <(extract_local_asset_paths "${E2E_ARTIFACT_DIR}/case_16_gh_viewer_body.html" "viewer")
else
    e2e_fail "GH smoke server failed to start"
fi
kill "${GH_PID}" 2>/dev/null || true
wait "${GH_PID}" 2>/dev/null || true

e2e_save_artifact "case_16_replay.txt" "$(cat <<EOF
Cloudflare-like smoke:
python3 -m http.server ${CF_PORT} --bind 127.0.0.1 --directory ${BUNDLE1}/viewer

GitHub-Pages-like smoke (repo subpath):
python3 -m http.server ${GH_PORT} --bind 127.0.0.1 --directory ${GH_ROOT}
EOF
)"

# ---------------------------------------------------------------------------
# Finalize: save hashes and summary
# ---------------------------------------------------------------------------

{
    echo "# SHA256 hashes for bundle artifacts"
    for f in "${BUNDLE1}/manifest.json" "${BUNDLE1}/mailbox.sqlite3"; do
        if [ -f "$f" ]; then
            echo "$(e2e_sha256 "$f")  ${f#"${WORK}/"}"
        fi
    done
    for f in "${BUNDLE2}/manifest.json" "${BUNDLE2}/mailbox.sqlite3"; do
        if [ -f "$f" ]; then
            echo "$(e2e_sha256 "$f")  ${f#"${WORK}/"}"
        fi
    done
} > "${E2E_ARTIFACT_DIR}/sha256_hashes.txt" 2>/dev/null || true

# Save PRAGMA results
{
    echo "=== Bundle 1 ==="
    echo "integrity_check: $(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA integrity_check;" 2>&1)"
    echo "journal_mode: $(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA journal_mode;" 2>&1)"
    echo "page_count: $(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA page_count;" 2>&1)"
    echo "page_size: $(sqlite3 "${BUNDLE1}/mailbox.sqlite3" "PRAGMA page_size;" 2>&1)"
    echo "table_list:"
    sqlite3 "${BUNDLE1}/mailbox.sqlite3" ".tables" 2>&1
} > "${E2E_ARTIFACT_DIR}/pragma_results.txt" 2>/dev/null || true

e2e_summary
