#!/usr/bin/env bash
# e2e_archive.sh - E2E test suite for git-archive side effects + notification signals
#
# Run via (authoritative):
#   am e2e run --project . archive
# Compatibility fallback:
#   AM_E2E_FORCE_LEGACY=1 ./scripts/e2e_test.sh archive
#
# This suite verifies:
# - Message archive writes (canonical + inbox/outbox copies) are byte-identical
# - Frontmatter format/parsing (`---json` + JSON object)
# - File reservation artifacts exist (sha1(path_pattern).json + id-<id>.json)
# - Attachment artifacts exist (webp + manifest + audit) and hashes match message references
# - Notification signals are emitted for to+cc only (not bcc) and cleared on fetch_inbox
#
# Artifacts:
#   tests/artifacts/archive/<timestamp>/*

set -euo pipefail

# Safety: default to keeping temp dirs so the shared harness doesn't run `rm -rf`.
: "${AM_E2E_KEEP_TMP:=1}"

E2E_SUITE="${E2E_SUITE:-archive}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=./e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Archive + Signals E2E Test Suite"

e2e_save_artifact "env_dump.txt" "$(e2e_dump_env 2>&1)"

for cmd in curl python3 git; do
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

wait_for_path() {
    local path="$1"
    local timeout_s="${2:-10}"
    local deadline=$(( $(date +%s) + timeout_s ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if [ -e "$path" ]; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

wait_for_git_contains() {
    local repo="$1"
    local needle="$2"
    local timeout_s="${3:-10}"
    local deadline=$(( $(date +%s) + timeout_s ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        if git -C "$repo" log -n 50 --pretty=%B 2>/dev/null | grep -F -q "$needle"; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

wait_for_git_clean() {
    local repo="$1"
    local timeout_s="${2:-10}"
    local deadline=$(( $(date +%s) + timeout_s ))
    while [ "$(date +%s)" -lt "$deadline" ]; do
        local status
        status="$(
            git -C "$repo" status --porcelain 2>/dev/null \
                | grep -vE '^\?\? search_index/?$' \
                || true
        )"
        if [ -z "$status" ]; then
            return 0
        fi
        sleep 0.05
    done
    return 1
}

sha1_str() {
    local s="$1"
    python3 - "$s" <<'PY'
import hashlib, sys
print(hashlib.sha1(sys.argv[1].encode("utf-8")).hexdigest())
PY
}

sha1_file() {
    local p="$1"
    python3 - "$p" <<'PY'
import hashlib, sys
p = sys.argv[1]
h = hashlib.sha1()
with open(p, "rb") as f:
    while True:
        b = f.read(1024 * 1024)
        if not b:
            break
        h.update(b)
print(h.hexdigest())
PY
}

extract_tool_text() {
    local resp_file="$1"
    python3 - <<'PY' "$resp_file"
import json, sys
data = json.load(open(sys.argv[1], "r", encoding="utf-8"))
if "error" in data and data["error"] is not None:
    print("")
    sys.exit(0)
res = data.get("result") or {}
content = res.get("content") or []
if content and isinstance(content[0], dict) and content[0].get("type") == "text":
    print(content[0].get("text") or "")
else:
    print(json.dumps(res))
PY
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

rpc_call() {
    local case_id="$1"
    local tool_name="$2"
    local args_json="$3"

    # Use the shared capture helper (request/response/headers/timing/status).
    e2e_mark_case_start "${case_id}"
    if ! e2e_rpc_call "${case_id}" "${API_URL}" "${tool_name}" "${args_json}" \
        "authorization: Bearer ${TOKEN}"; then
        local status_file_new="${E2E_ARTIFACT_DIR}/${case_id}/status.txt"
        local status="unknown"
        if [ -f "${status_file_new}" ]; then
            status="$(cat "${status_file_new}")"
        fi
        e2e_fatal "${case_id}: unexpected HTTP status ${status}"
    fi

    # Backward-compatible flat artifact paths used by existing assertions.
    local case_dir="${E2E_ARTIFACT_DIR}/${case_id}"
    cp "${case_dir}/request.json" "${E2E_ARTIFACT_DIR}/${case_id}_request.json"
    cp "${case_dir}/response.json" "${E2E_ARTIFACT_DIR}/${case_id}_body.json"
    cp "${case_dir}/headers.txt" "${E2E_ARTIFACT_DIR}/${case_id}_headers.txt"
    cp "${case_dir}/status.txt" "${E2E_ARTIFACT_DIR}/${case_id}_status.txt"
    cp "${case_dir}/timing.txt" "${E2E_ARTIFACT_DIR}/${case_id}_timing.txt"
    cp "${case_dir}/curl_stderr.txt" "${E2E_ARTIFACT_DIR}/${case_id}_curl_stderr.txt"
}

parse_frontmatter_json() {
    local md_path="$1"
    python3 - <<'PY' "$md_path"
import json, sys
text = open(sys.argv[1], "r", encoding="utf-8").read()
start = text.find("---json\n")
if start < 0:
    raise SystemExit("missing ---json marker")
end = text.find("\n---\n", start + 7)
if end < 0:
    raise SystemExit("missing closing --- delimiter")
payload = text[start + len("---json\n"): end]
print(payload)
PY
}

json_get() {
    return 0
}

e2e_log "Allocating workspace"
WORK="$(e2e_mktemp "e2e_archive")"
DB_PATH="${WORK}/db.sqlite3"
STORAGE_ROOT="${WORK}/storage_root"
SIGNALS_DIR="${WORK}/signals"
TOKEN="e2e-token"

if ! e2e_start_server_with_logs "${DB_PATH}" "${STORAGE_ROOT}" "archive" \
    "HTTP_PATH=/api" \
    "HTTP_BEARER_TOKEN=${TOKEN}" \
    "HTTP_ALLOW_LOCALHOST_UNAUTHENTICATED=0" \
    "HTTP_RBAC_ENABLED=0" \
    "HTTP_RATE_LIMIT_ENABLED=0" \
    "ALLOW_ABSOLUTE_ATTACHMENT_PATHS=1" \
    "NOTIFICATIONS_ENABLED=1" \
    "NOTIFICATIONS_SIGNALS_DIR=${SIGNALS_DIR}" \
    "NOTIFICATIONS_INCLUDE_METADATA=1" \
    "NOTIFICATIONS_DEBOUNCE_MS=0"; then
    e2e_fatal "server failed to start (port not open)"
fi
trap 'e2e_stop_server || true' EXIT

API_URL="${E2E_SERVER_URL%/mcp/}/api/"

e2e_case_banner "ensure_project + register agents"
PROJECT_DIR="$(e2e_mktemp "e2e_archive_project")"
rpc_call "ensure_project" "ensure_project" "{\"human_key\": \"${PROJECT_DIR}\"}"
if rpc_has_error "${E2E_ARTIFACT_DIR}/ensure_project_body.json"; then
    e2e_fatal "ensure_project returned JSON-RPC error"
fi
PROJECT_JSON="$(extract_tool_text "${E2E_ARTIFACT_DIR}/ensure_project_body.json")"
PROJECT_SLUG="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['slug'])" "$PROJECT_JSON")"
e2e_save_artifact "project.json" "$PROJECT_JSON"

# Register: sender inline + sender file + recipients (to/cc/bcc)
rpc_call "register_sender_inline" "register_agent" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'program': 'e2e', 'model': 'test', 'name': 'RedFox', 'task_description': 'e2e', 'attachments_policy': 'inline'}))" "${PROJECT_DIR}")"
rpc_call "register_sender_file" "register_agent" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'program': 'e2e', 'model': 'test', 'name': 'GreenCastle', 'task_description': 'e2e', 'attachments_policy': 'file'}))" "${PROJECT_DIR}")"
rpc_call "register_to" "register_agent" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'program': 'e2e', 'model': 'test', 'name': 'BlueBear', 'task_description': 'e2e'}))" "${PROJECT_DIR}")"
rpc_call "register_cc" "register_agent" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'program': 'e2e', 'model': 'test', 'name': 'OrangeFox', 'task_description': 'e2e'}))" "${PROJECT_DIR}")"
rpc_call "register_bcc" "register_agent" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'program': 'e2e', 'model': 'test', 'name': 'SilverCove', 'task_description': 'e2e'}))" "${PROJECT_DIR}")"
rpc_call "set_policy_redfox_open" "set_contact_policy" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'agent_name': 'RedFox', 'policy': 'open'}))" "${PROJECT_DIR}")"
if rpc_has_error "${E2E_ARTIFACT_DIR}/set_policy_redfox_open_body.json"; then
    e2e_fatal "set_contact_policy(open) failed for RedFox"
fi
rpc_call "set_policy_greencastle_open" "set_contact_policy" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'agent_name': 'GreenCastle', 'policy': 'open'}))" "${PROJECT_DIR}")"
if rpc_has_error "${E2E_ARTIFACT_DIR}/set_policy_greencastle_open_body.json"; then
    e2e_fatal "set_contact_policy(open) failed for GreenCastle"
fi

e2e_pass "seeded project_slug=${PROJECT_SLUG}"

# Wait for git repo init at storage root
if ! wait_for_path "${STORAGE_ROOT}/.git" 10; then
    e2e_fatal "archive root did not initialize as git repo"
fi

e2e_save_artifact "archive_tree_after_seed.txt" "$(e2e_tree "${STORAGE_ROOT}" 2>&1 || true)"

e2e_case_banner "send_message rejects empty recipients"
before_msg_count="$(
    (find "${STORAGE_ROOT}/projects/${PROJECT_SLUG}/messages" -type f -name '*.md' 2>/dev/null || true) \
        | wc -l | tr -d ' '
)"
rpc_call "send_empty" "send_message" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'sender_name': 'RedFox', 'to': [], 'subject': 'empty recipients', 'body_md': 'nope'}))" "${PROJECT_DIR}")"
if ! rpc_has_error "${E2E_ARTIFACT_DIR}/send_empty_body.json"; then
    e2e_fatal "expected JSON-RPC error for empty recipients"
fi
after_msg_count="$(
    (find "${STORAGE_ROOT}/projects/${PROJECT_SLUG}/messages" -type f -name '*.md' 2>/dev/null || true) \
        | wc -l | tr -d ' '
)"
e2e_assert_eq "no messages written for error case" "${before_msg_count}" "${after_msg_count}"

e2e_case_banner "send_message (single recipient) writes canonical + inbox/outbox"
rpc_call "send_one" "send_message" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'sender_name': 'RedFox', 'to': ['BlueBear'], 'subject': 'archive single', 'body_md': 'hello', 'thread_id': 'br-2ei.9.2'}))" "${PROJECT_DIR}")"
if rpc_has_error "${E2E_ARTIFACT_DIR}/send_one_body.json"; then
    e2e_fatal "send_message(single) returned JSON-RPC error"
fi
SEND_ONE_TEXT="$(extract_tool_text "${E2E_ARTIFACT_DIR}/send_one_body.json")"
MSG1_ID="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['deliveries'][0]['payload']['id'])" "${SEND_ONE_TEXT}")"

# Wait for canonical message file
deadline=$(( $(date +%s) + 10 ))
CANON1=""
while [ "$(date +%s)" -lt "$deadline" ]; do
    CANON1="$(find "${STORAGE_ROOT}/projects/${PROJECT_SLUG}/messages" -type f -name "*__${MSG1_ID}.md" 2>/dev/null | head -n 1 || true)"
    if [ -n "$CANON1" ] && [ -f "$CANON1" ]; then
        break
    fi
    sleep 0.05
done
if [ -z "$CANON1" ] || [ ! -f "$CANON1" ]; then
    e2e_fatal "canonical message file for id=${MSG1_ID} not found"
fi

y="$(basename "$(dirname "$(dirname "$CANON1")")")"
m="$(basename "$(dirname "$CANON1")")"
filename="$(basename "$CANON1")"
OUTBOX1="${STORAGE_ROOT}/projects/${PROJECT_SLUG}/agents/RedFox/outbox/${y}/${m}/${filename}"
INBOX1="${STORAGE_ROOT}/projects/${PROJECT_SLUG}/agents/BlueBear/inbox/${y}/${m}/${filename}"

e2e_assert_file_exists "canonical exists" "$CANON1"
e2e_assert_file_exists "outbox exists" "$OUTBOX1"
e2e_assert_file_exists "inbox exists" "$INBOX1"

canon_sha="$(e2e_sha256 "$CANON1")"
outbox_sha="$(e2e_sha256 "$OUTBOX1")"
inbox_sha="$(e2e_sha256 "$INBOX1")"
e2e_assert_eq "outbox matches canonical" "$canon_sha" "$outbox_sha"
e2e_assert_eq "inbox matches canonical" "$canon_sha" "$inbox_sha"

fm1="$(parse_frontmatter_json "$CANON1")"
python3 -c "import json,sys; json.loads(sys.argv[1]); print('ok')" "$fm1" >/dev/null 2>&1 || e2e_fatal "frontmatter JSON parse failed"
e2e_pass "frontmatter JSON parses"

if ! wait_for_git_contains "${STORAGE_ROOT}" "mail: RedFox -> BlueBear" 10; then
    e2e_fatal "git log did not include mail commit for single-recipient message"
fi

e2e_case_banner "send_message (to+cc+bcc) emits signals for to+cc only"
rpc_call "send_multi" "send_message" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'sender_name': 'GreenCastle', 'to': ['BlueBear'], 'cc': ['OrangeFox'], 'bcc': ['SilverCove'], 'subject': 'archive multi', 'body_md': 'multi', 'thread_id': 'br-2ei.9.2'}))" "${PROJECT_DIR}")"
if rpc_has_error "${E2E_ARTIFACT_DIR}/send_multi_body.json"; then
    e2e_fatal "send_message(multi) returned JSON-RPC error"
fi
SEND_MULTI_TEXT="$(extract_tool_text "${E2E_ARTIFACT_DIR}/send_multi_body.json")"
MSG2_ID="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['deliveries'][0]['payload']['id'])" "${SEND_MULTI_TEXT}")"

TO_SIGNAL="${SIGNALS_DIR}/projects/${PROJECT_SLUG}/agents/BlueBear.signal"
CC_SIGNAL="${SIGNALS_DIR}/projects/${PROJECT_SLUG}/agents/OrangeFox.signal"
BCC_SIGNAL="${SIGNALS_DIR}/projects/${PROJECT_SLUG}/agents/SilverCove.signal"

wait_for_path "$TO_SIGNAL" 5 || e2e_fatal "missing to-signal file for BlueBear"
wait_for_path "$CC_SIGNAL" 5 || e2e_fatal "missing cc-signal file for OrangeFox"
if [ -f "$BCC_SIGNAL" ]; then
    e2e_fatal "bcc-signal unexpectedly exists for SilverCove"
fi
e2e_pass "signals present for to+cc only"

to_signal_json="$(cat "$TO_SIGNAL")"
python3 -c "import json,sys; obj=json.loads(sys.argv[1]); assert obj['project']==sys.argv[2]; assert obj['agent']=='BlueBear'; assert obj.get('message',{}).get('id')==int(sys.argv[3]); print('ok')" "$to_signal_json" "$PROJECT_SLUG" "$MSG2_ID" >/dev/null 2>&1 || e2e_fatal "to-signal JSON mismatch"

e2e_case_banner "fetch_inbox clears the agent's signal"
rpc_call "fetch_inbox" "fetch_inbox" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'agent_name': 'BlueBear', 'limit': 50}))" "${PROJECT_DIR}")"

# Clear is synchronous but give it a brief moment for FS visibility.
deadline=$(( $(date +%s) + 5 ))
while [ "$(date +%s)" -lt "$deadline" ]; do
    if [ ! -f "$TO_SIGNAL" ]; then
        break
    fi
    sleep 0.05
done
if [ -f "$TO_SIGNAL" ]; then
    e2e_fatal "to-signal was not cleared after fetch_inbox"
fi
if [ ! -f "$CC_SIGNAL" ]; then
    e2e_fatal "cc-signal disappeared unexpectedly"
fi
e2e_pass "fetch_inbox cleared to-signal only"

e2e_case_banner "file_reservation_paths writes sha1 + id artifacts"
PAT_EXCL="src/**/*.rs"
PAT_SHARED="tests/e2e/*"

rpc_call "reserve_excl" "file_reservation_paths" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'agent_name': 'RedFox', 'paths': [sys.argv[2]], 'ttl_seconds': 3600, 'exclusive': True, 'reason': 'br-2ei.9.2'}))" "${PROJECT_DIR}" "${PAT_EXCL}")"
RES_EXCL_TEXT="$(extract_tool_text "${E2E_ARTIFACT_DIR}/reserve_excl_body.json")"
RES_EXCL_ID="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['granted'][0]['id'])" "${RES_EXCL_TEXT}")"

rpc_call "reserve_shared" "file_reservation_paths" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'agent_name': 'RedFox', 'paths': [sys.argv[2]], 'ttl_seconds': 3600, 'exclusive': False, 'reason': 'br-2ei.9.2'}))" "${PROJECT_DIR}" "${PAT_SHARED}")"
RES_SHARED_TEXT="$(extract_tool_text "${E2E_ARTIFACT_DIR}/reserve_shared_body.json")"
RES_SHARED_ID="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['granted'][0]['id'])" "${RES_SHARED_TEXT}")"

digest_excl="$(sha1_str "${PAT_EXCL}")"
digest_shared="$(sha1_str "${PAT_SHARED}")"
EXCL_SHA_PATH="${STORAGE_ROOT}/projects/${PROJECT_SLUG}/file_reservations/${digest_excl}.json"
EXCL_ID_PATH="${STORAGE_ROOT}/projects/${PROJECT_SLUG}/file_reservations/id-${RES_EXCL_ID}.json"
SHARED_SHA_PATH="${STORAGE_ROOT}/projects/${PROJECT_SLUG}/file_reservations/${digest_shared}.json"
SHARED_ID_PATH="${STORAGE_ROOT}/projects/${PROJECT_SLUG}/file_reservations/id-${RES_SHARED_ID}.json"

wait_for_path "$EXCL_SHA_PATH" 10 || e2e_fatal "missing reservation sha artifact (exclusive)"
wait_for_path "$EXCL_ID_PATH" 10 || e2e_fatal "missing reservation id artifact (exclusive)"
wait_for_path "$SHARED_SHA_PATH" 10 || e2e_fatal "missing reservation sha artifact (shared)"
wait_for_path "$SHARED_ID_PATH" 10 || e2e_fatal "missing reservation id artifact (shared)"
e2e_pass "reservation artifacts exist"

if ! wait_for_git_contains "${STORAGE_ROOT}" "file_reservation: RedFox" 10; then
    e2e_fatal "git log did not include file_reservation commit"
fi

rpc_call "renew_res" "renew_file_reservations" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'agent_name': 'RedFox', 'extend_seconds': 60, 'file_reservation_ids': [int(sys.argv[2])]}))" "${PROJECT_DIR}" "${RES_EXCL_ID}")"
rpc_call "release_res" "release_file_reservations" "$(python3 -c "import json,sys; print(json.dumps({'project_key': sys.argv[1], 'agent_name': 'RedFox', 'file_reservation_ids': [int(sys.argv[2])]}))" "${PROJECT_DIR}" "${RES_EXCL_ID}")"
e2e_pass "renew + release executed"

e2e_case_banner "attachments: inline markdown + file-backed attachment"

# Write a tiny deterministic 1x1 PNG.
IMG_PATH="${WORK}/dot.png"
python3 - <<'PY' "$IMG_PATH"
import struct
import sys
import zlib

def chunk(kind: bytes, data: bytes) -> bytes:
    return (
        struct.pack(">I", len(data))
        + kind
        + data
        + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    )

path = sys.argv[1]
width = 1
height = 1

# Raw image bytes: filter=0 + RGBA(0,0,0,0)
raw = b"\x00\x00\x00\x00\x00"
compressed = zlib.compress(raw, level=9)

png = b"".join(
    [
        b"\x89PNG\r\n\x1a\n",
        chunk(
            b"IHDR",
            struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0),
        ),
        chunk(b"IDAT", compressed),
        chunk(b"IEND", b""),
    ]
)

with open(path, "wb") as f:
    f.write(png)
PY

IMG_SHA1="$(sha1_file "$IMG_PATH")"
e2e_save_artifact "attachment_sha1.txt" "$IMG_SHA1"

rpc_call "send_inline_img" "send_message" "$(
    python3 - <<'PY' "${PROJECT_DIR}" "${IMG_PATH}"
import json, sys
project = sys.argv[1]
img = sys.argv[2]
print(json.dumps({
  "project_key": project,
  "sender_name": "RedFox",
  "to": ["BlueBear"],
  "subject": "inline image",
  "body_md": f"![dot]({img})",
  "thread_id": "br-2ei.9.2",
}))
PY
)"
if rpc_has_error "${E2E_ARTIFACT_DIR}/send_inline_img_body.json"; then
    e2e_fatal "send_message(inline img) returned JSON-RPC error"
fi
SEND_INLINE_TEXT="$(extract_tool_text "${E2E_ARTIFACT_DIR}/send_inline_img_body.json")"
MSG3_ID="$(python3 - <<'PY' "${SEND_INLINE_TEXT}"
import json, sys
print(json.loads(sys.argv[1])["deliveries"][0]["payload"]["id"])
PY
)"

deadline=$(( $(date +%s) + 10 ))
CANON3=""
while [ "$(date +%s)" -lt "$deadline" ]; do
    CANON3="$(find "${STORAGE_ROOT}/projects/${PROJECT_SLUG}/messages" -type f -name "*__${MSG3_ID}.md" 2>/dev/null | head -n 1 || true)"
    if [ -n "$CANON3" ] && [ -f "$CANON3" ]; then
        break
    fi
    sleep 0.05
done
if [ -z "$CANON3" ] || [ ! -f "$CANON3" ]; then
    e2e_fatal "canonical message file for inline attachment not found"
fi

fm3="$(parse_frontmatter_json "$CANON3")"
python3 - <<'PY' "$fm3" "$IMG_SHA1" >/dev/null
import json, sys
fm = json.loads(sys.argv[1])
sha_expected = sys.argv[2]
atts = fm.get("attachments") or []
assert isinstance(atts, list) and atts, "expected non-empty attachments"
sha = atts[0].get("sha1")
assert sha == sha_expected, f"sha1 mismatch: {sha} != {sha_expected}"
assert atts[0].get("data_base64"), "expected inline attachment to include data_base64"
PY
e2e_pass "inline attachment metadata matches sha1"

MANIFEST_PATH="${STORAGE_ROOT}/projects/${PROJECT_SLUG}/attachments/_manifests/${IMG_SHA1}.json"
AUDIT_PATH="${STORAGE_ROOT}/projects/${PROJECT_SLUG}/attachments/_audit/${IMG_SHA1}.log"
wait_for_path "$MANIFEST_PATH" 10 || e2e_fatal "missing attachment manifest"
wait_for_path "$AUDIT_PATH" 10 || e2e_fatal "missing attachment audit log"

WEBP_REL="$(python3 - <<'PY' "$MANIFEST_PATH"
import json, sys
print(json.load(open(sys.argv[1], "r", encoding="utf-8"))["webp_path"])
PY
)"
wait_for_path "${STORAGE_ROOT}/${WEBP_REL}" 10 || e2e_fatal "missing webp file referenced by manifest: ${WEBP_REL}"
e2e_pass "attachment files exist (webp + manifest + audit)"

rpc_call "send_file_img" "send_message" "$(
    python3 - <<'PY' "${PROJECT_DIR}" "${IMG_PATH}"
import json, sys
project = sys.argv[1]
img = sys.argv[2]
print(json.dumps({
  "project_key": project,
  "sender_name": "GreenCastle",
  "to": ["BlueBear"],
  "subject": "file attachment",
  "body_md": "see attachment",
  "thread_id": "br-2ei.9.2",
  "attachment_paths": [img],
}))
PY
)"
if rpc_has_error "${E2E_ARTIFACT_DIR}/send_file_img_body.json"; then
    e2e_fatal "send_message(file img) returned JSON-RPC error"
fi
SEND_FILE_TEXT="$(extract_tool_text "${E2E_ARTIFACT_DIR}/send_file_img_body.json")"
MSG4_ID="$(python3 - <<'PY' "${SEND_FILE_TEXT}"
import json, sys
print(json.loads(sys.argv[1])["deliveries"][0]["payload"]["id"])
PY
)"

deadline=$(( $(date +%s) + 10 ))
CANON4=""
while [ "$(date +%s)" -lt "$deadline" ]; do
    CANON4="$(find "${STORAGE_ROOT}/projects/${PROJECT_SLUG}/messages" -type f -name "*__${MSG4_ID}.md" 2>/dev/null | head -n 1 || true)"
    if [ -n "$CANON4" ] && [ -f "$CANON4" ]; then
        break
    fi
    sleep 0.05
done
if [ -z "$CANON4" ] || [ ! -f "$CANON4" ]; then
    e2e_fatal "canonical message file for file attachment not found"
fi

fm4="$(parse_frontmatter_json "$CANON4")"
FILE_PATH_REL="$(python3 - <<'PY' "$fm4"
import json, sys
fm = json.loads(sys.argv[1])
atts = fm.get("attachments") or []
print(atts[0].get("path") or "")
PY
)"
if [ -z "$FILE_PATH_REL" ]; then
    e2e_fatal "expected file attachment to include path"
fi
wait_for_path "${STORAGE_ROOT}/${FILE_PATH_REL}" 10 || e2e_fatal "missing file-backed attachment path: ${FILE_PATH_REL}"
e2e_pass "file-backed attachment path exists"

# Archive should be fully committed (no dirty/untracked files) after WBQ + commit coalescer drain.
if ! wait_for_git_clean "${STORAGE_ROOT}" 10; then
    e2e_save_artifact "git_status_porcelain.txt" "$(git -C "${STORAGE_ROOT}" status --porcelain 2>&1 || true)"
    e2e_fatal "archive git repo not clean after operations (uncommitted artifacts?)"
fi
e2e_pass "archive repo clean"

e2e_save_artifact "archive_tree_final.txt" "$(e2e_tree "${STORAGE_ROOT}" 2>&1 || true)"
e2e_save_artifact "git_log_oneline.txt" "$(git -C "${STORAGE_ROOT}" log -n 50 --oneline 2>&1 || true)"

# Hash large-ish artifacts for debug (>= 10KB)
{
    echo "# sha256 for files >= 10KB (excluding .git)"
    find "${STORAGE_ROOT}" -type f -not -path '*/.git/*' -printf '%s %p\n' 2>/dev/null \
        | awk '$1 >= 10240 {print $2}' \
        | sort \
        | while read -r p; do
            echo "$(e2e_sha256 "$p")  ${p#"${STORAGE_ROOT}/"}"
        done
} > "${E2E_ARTIFACT_DIR}/sha256_large_files.txt" 2>/dev/null || true

e2e_summary
