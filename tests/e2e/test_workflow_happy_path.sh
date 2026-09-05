#!/usr/bin/env bash
# test_workflow_happy_path.sh - P0 E2E: Canonical agent workflow from AGENTS.md
#
# THE most important E2E test. Exercises the exact workflow documented in
# AGENTS.md "Same Repository Workflow" section, which every real agent follows:
#
#   1. ensure_project → register_agent → file_reservation_paths
#   2. send_message → fetch_inbox → acknowledge_message → reply_message
#   3. resource://inbox → resource://thread
#   4. release_file_reservations → verify archive + DB
#   5. Macro equivalents: macro_start_session → macro_file_reservation_cycle
#   6. Offline CLI reserve → guard conflict → renew → release → guard clear
#
# Target: 30+ assertions

export E2E_SUITE="workflow_happy_path"
: "${AM_E2E_KEEP_TMP:=1}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Canonical Agent Workflow E2E Suite (P0)"

e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"

WORK="$(e2e_mktemp "e2e_workflow")"
WF_DB="${WORK}/workflow_test.sqlite3"
WF_STORAGE="${WORK}/storage"
mkdir -p "$WF_STORAGE"
PROJECT_PATH="/tmp/e2e_workflow_project_$$"

INIT_REQ='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e-workflow","version":"1.0"}}}'

# ---------------------------------------------------------------------------
# Shared helpers (same pattern as test_macros.sh / test_stdio.sh)
# ---------------------------------------------------------------------------

send_jsonrpc_session() {
    local db_path="$1"
    local storage_root="$2"
    shift 2
    AM_E2E_ARTIFACT_DIR="$E2E_ARTIFACT_DIR" \
    python3 - "$db_path" "$storage_root" "$WORK" "$(command -v am)" \
        "${WORKFLOW_SESSION_TIMEOUT_S:-45}" "$@" <<'PY'
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import subprocess
import sys
import tempfile
import time

db, storage, work, binary, timeout, *raw_requests = sys.argv[1:]
requests = [json.loads(raw) for raw in raw_requests]
ids = [request.get("id") for request in requests]
if not requests or len(set(ids)) != len(ids) or None in ids:
    raise SystemExit("workflow requires a nonempty set of unique request IDs")
session_base = Path(os.environ.get("AM_E2E_ARTIFACT_DIR", work)) / "workflow_sessions"
session_base.mkdir(parents=True, exist_ok=True)
session = Path(tempfile.mkdtemp(prefix="session-", dir=session_base))
env = os.environ.copy()
env.pop("AM_INTERFACE_MODE", None)
env.update(DATABASE_URL="sqlite:///" + db, STORAGE_ROOT=storage,
           RUST_LOG=os.environ.get("WORKFLOW_RUST_LOG", "error"),
           AM_ATC_ENABLED="false", AM_ATC_WRITE_MODE="off", ATC_LEARNING_DISABLED="1",
           LLM_ENABLED="false", NOTIFICATIONS_ENABLED="false", TUI_ENABLED="false")
started = time.monotonic()
deadline = started + float(timeout)
with open(binary, "rb") as executable:
    binary_sha256 = hashlib.file_digest(executable, "sha256").hexdigest()
summary = {"run_id": session.name, "binary": binary, "binary_sha256": binary_sha256,
           "requested_ids": ids, "completed_ids": [], "client_pid": os.getpid(),
           "passed": False, "shutdown": "not-started"}
buffer = b""
recorded = 0

def interrupted(signum, frame):
    raise InterruptedError(f"workflow interrupted by signal {signum}")

signal.signal(signal.SIGTERM, interrupted)
with (session / "stderr.txt").open("wb") as stderr, \
     (session / "history.jsonl").open("w") as history, \
     (session / "transcript.jsonl").open("x") as transcript:
    proc = subprocess.Popen([binary, "serve-stdio"], cwd=session, env=env,
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=stderr, start_new_session=True)
    summary["server_pid"] = proc.pid
    selector = selectors.DefaultSelector()
    selector.register(proc.stdout, selectors.EVENT_READ)

    def event(kind, payload):
        # Raw synthetic fixture responses stay in the private session files;
        # the operation history records stable IDs and content digests only.
        transcript.write(json.dumps({"event": kind, "payload": payload}) + "\n")
        transcript.flush()
        history.write(json.dumps({"event": kind, "id": payload.get("id"),
            "method": payload.get("method"), "elapsed_s": time.monotonic() - started,
            "sha256": hashlib.sha256(json.dumps(payload, sort_keys=True).encode()).hexdigest()}) + "\n")
        history.flush()

    def send(payload):
        event("invoke", payload)
        proc.stdin.write(json.dumps(payload).encode() + b"\n")
        proc.stdin.flush()

    try:
        for request in requests:
            send(request)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"missing terminal response for id {request['id']}")
                if b"\n" not in buffer:
                    if not selector.select(min(remaining, 1)):
                        continue
                    chunk = os.read(proc.stdout.fileno(), 65536)
                    if not chunk:
                        raise RuntimeError(f"EOF before response id {request['id']}")
                    recorded += len(chunk)
                    if recorded > 8 * 1024 * 1024:
                        raise RuntimeError("workflow response budget exceeded")
                    buffer += chunk
                    continue
                line, buffer = buffer.split(b"\n", 1)
                if not line.strip():
                    continue
                response = json.loads(line)
                event("complete" if "id" in response else "notification", response)
                print(json.dumps(response), flush=True)
                if "id" not in response:
                    continue
                if response["id"] != request["id"]:
                    raise RuntimeError("unexpected response ID")
                if "result" not in response or "error" in response or response["result"].get("isError"):
                    raise RuntimeError(f"request id {request['id']} failed")
                summary["completed_ids"].append(request["id"])
                break
            if request["method"] == "initialize":
                send({"jsonrpc": "2.0", "method": "notifications/initialized"})
        summary["passed"] = True
    except Exception as error:
        summary["error"] = str(error)
        print(str(error), file=sys.stderr)
    finally:
        try:
            proc.stdin.close()
        except OSError:
            pass
        selector.close()
        try:
            proc.wait(timeout=20)
            summary["shutdown"] = "graceful"
        except subprocess.TimeoutExpired:
            summary["passed"] = False
            summary["shutdown"] = "forced"
            os.killpg(proc.pid, signal.SIGTERM)
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(proc.pid, signal.SIGKILL)
                proc.wait(timeout=5)
        summary["server_exit"] = proc.returncode
        summary["passed"] = summary["passed"] and proc.returncode == 0
        (session / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(f"workflow session: {session}", file=sys.stderr)
print(json.dumps({"workflow_session": {"passed": summary["passed"],
                                      "completed_ids": summary["completed_ids"]}}), flush=True)
if not summary["passed"]:
    raise SystemExit(1)
PY
}

extract_result() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == $req_id and 'result' in d:
            content = d['result'].get('content', [])
            if content:
                print(content[0].get('text', ''))
                sys.exit(0)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
" 2>/dev/null
}

is_error_result() {
    local response="$1"
    local req_id="$2"
    echo "$response" | python3 -c "
import sys, json
responses = []
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if isinstance(d, dict):
            responses.append(d)
    except (json.JSONDecodeError, KeyError, IndexError):
        pass
sessions = [d['workflow_session'] for d in responses if 'workflow_session' in d]
session = sessions[0] if len(sessions) == 1 else None
if not isinstance(session, dict) or session.get('passed') is not True or not isinstance(session.get('completed_ids'), list) or $req_id not in session['completed_ids']:
    print('true')
    sys.exit(0)
matching = [d for d in responses if d.get('id') == $req_id]
if len(matching) == 1:
    d = matching[0]
    if 'error' not in d and isinstance(d.get('result'), dict) and not d['result'].get('isError'):
        print('false')
        sys.exit(0)
print('true')
" 2>/dev/null
}

# Parse a JSON field from extracted result text
parse_json_field() {
    local text="$1"
    local field="$2"
    echo "$text" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    val = d
    for key in '$field'.split('.'):
        if isinstance(val, dict):
            val = val.get(key, '')
        elif isinstance(val, list) and key.isdigit():
            val = val[int(key)]
        else:
            val = ''
            break
    print(val if val is not None else '')
except Exception:
    print('')
" 2>/dev/null
}

# ===========================================================================
# Phase 1: Project setup + agent registration
# ===========================================================================
e2e_case_banner "Phase 1: ensure_project + register two agents"

PHASE1_RESP="$(send_jsonrpc_session "$WF_DB" "$WF_STORAGE" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":10,\"method\":\"tools/call\",\"params\":{\"name\":\"ensure_project\",\"arguments\":{\"human_key\":\"${PROJECT_PATH}\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":11,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"RedFox\"}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":12,\"method\":\"tools/call\",\"params\":{\"name\":\"register_agent\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"name\":\"BluePeak\"}}}" \
)"
e2e_save_artifact "phase1_setup.txt" "$PHASE1_RESP"

# Verify ensure_project
EP_TEXT="$(extract_result "$PHASE1_RESP" 10)"
EP_ERROR="$(is_error_result "$PHASE1_RESP" 10)"
if [ "$EP_ERROR" = "true" ]; then
    e2e_fail "ensure_project returned error"
    echo "    text: $EP_TEXT"
else
    e2e_pass "ensure_project succeeded"
fi

EP_SLUG="$(parse_json_field "$EP_TEXT" "slug")"
if [ -n "$EP_SLUG" ]; then
    e2e_pass "ensure_project returned slug: $EP_SLUG"
else
    e2e_fail "ensure_project missing slug in response"
fi

# Verify register_agent for RedFox
RF_ERROR="$(is_error_result "$PHASE1_RESP" 11)"
if [ "$RF_ERROR" = "true" ]; then
    e2e_fail "register_agent RedFox returned error"
else
    e2e_pass "register_agent RedFox succeeded"
fi

RF_TEXT="$(extract_result "$PHASE1_RESP" 11)"
RF_NAME="$(parse_json_field "$RF_TEXT" "name")"
e2e_assert_eq "RedFox agent name" "RedFox" "$RF_NAME"

# Verify register_agent for BluePeak
BP_ERROR="$(is_error_result "$PHASE1_RESP" 12)"
if [ "$BP_ERROR" = "true" ]; then
    e2e_fail "register_agent BluePeak returned error"
else
    e2e_pass "register_agent BluePeak succeeded"
fi

# ===========================================================================
# Phase 2: File reservations
# ===========================================================================
e2e_case_banner "Phase 2: file_reservation_paths (exclusive)"

PHASE2_RESP="$(send_jsonrpc_session "$WF_DB" "$WF_STORAGE" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":20,\"method\":\"tools/call\",\"params\":{\"name\":\"file_reservation_paths\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"RedFox\",\"paths\":[\"src/lib.rs\",\"src/main.rs\"],\"ttl_seconds\":3600,\"exclusive\":true,\"reason\":\"br-3h13.7.8 testing\"}}}" \
)"
e2e_save_artifact "phase2_reserve.txt" "$PHASE2_RESP"

RES_TEXT="$(extract_result "$PHASE2_RESP" 20)"
RES_ERROR="$(is_error_result "$PHASE2_RESP" 20)"
if [ "$RES_ERROR" = "true" ]; then
    e2e_fail "file_reservation_paths returned error"
    echo "    text: $RES_TEXT"
else
    e2e_pass "file_reservation_paths succeeded"
fi

RES_CHECK="$(echo "$RES_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    granted = d.get('granted', [])
    conflicts = d.get('conflicts', [])
    paths = [g.get('path_pattern', '') for g in granted]
    print(f'granted={len(granted)}|conflicts={len(conflicts)}|paths={\",\".join(paths)}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_assert_contains "2 paths granted" "$RES_CHECK" "granted=2"
e2e_assert_contains "no conflicts" "$RES_CHECK" "conflicts=0"
e2e_assert_contains "src/lib.rs reserved" "$RES_CHECK" "src/lib.rs"
e2e_assert_contains "src/main.rs reserved" "$RES_CHECK" "src/main.rs"

# ===========================================================================
# Phase 3: Messaging — send, fetch inbox, acknowledge, reply
# ===========================================================================
e2e_case_banner "Phase 3: send → fetch_inbox → acknowledge → reply"

PHASE3_RESP="$(send_jsonrpc_session "$WF_DB" "$WF_STORAGE" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":30,\"method\":\"tools/call\",\"params\":{\"name\":\"send_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"sender_name\":\"RedFox\",\"to\":[\"BluePeak\"],\"subject\":\"Implementation update\",\"body_md\":\"## Progress\\n\\nAll tests passing. Ready for review.\",\"thread_id\":\"FEAT-42\",\"ack_required\":true}}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":31,\"method\":\"tools/call\",\"params\":{\"name\":\"fetch_inbox\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BluePeak\",\"include_bodies\":true,\"limit\":10}}}" \
)"
e2e_save_artifact "phase3_send_fetch.txt" "$PHASE3_RESP"

# Verify send_message
SEND_ERROR="$(is_error_result "$PHASE3_RESP" 30)"
SEND_TEXT="$(extract_result "$PHASE3_RESP" 30)"
if [ "$SEND_ERROR" = "true" ]; then
    e2e_fail "send_message returned error"
    echo "    text: $SEND_TEXT"
else
    e2e_pass "send_message succeeded"
fi

# send_message returns {deliveries: [{project, payload: {id, ...}}], count}
MSG_ID="$(parse_json_field "$SEND_TEXT" "deliveries.0.payload.id")"
if [ -n "$MSG_ID" ] && [ "$MSG_ID" != "None" ]; then
    e2e_pass "send_message returned message id: $MSG_ID"
else
    e2e_fail "send_message missing id in response"
fi

# Verify fetch_inbox
INBOX_ERROR="$(is_error_result "$PHASE3_RESP" 31)"
INBOX_TEXT="$(extract_result "$PHASE3_RESP" 31)"
if [ "$INBOX_ERROR" = "true" ]; then
    e2e_fail "fetch_inbox returned error"
    echo "    text: $INBOX_TEXT"
else
    e2e_pass "fetch_inbox succeeded"
fi

INBOX_CHECK="$(echo "$INBOX_TEXT" | python3 -c "
import sys, json
try:
    messages = json.loads(sys.stdin.read())
    if isinstance(messages, list):
        count = len(messages)
        subjects = [m.get('subject', '') for m in messages]
        senders = [m.get('from', '') for m in messages]
        bodies = [m.get('body_md', '') for m in messages]
        target_count = sum(1 for s in subjects if 'Implementation update' in s)
        has_target = target_count > 0
        has_sender = 'RedFox' in senders
        has_body = any('tests passing' in b for b in bodies)
        thread_ids = [m.get('thread_id', '') for m in messages]
        has_thread = 'FEAT-42' in thread_ids
        print(f'count={count}|target_count={target_count}|has_target={has_target}|has_sender={has_sender}|has_body={has_body}|has_thread={has_thread}')
    else:
        print(f'not_list|type={type(messages).__name__}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_save_artifact "phase3_inbox_parsed.txt" "$INBOX_CHECK"

e2e_assert_contains "inbox includes one target message" "$INBOX_CHECK" "target_count=1"
e2e_assert_contains "correct subject" "$INBOX_CHECK" "has_target=True"
e2e_assert_contains "correct sender" "$INBOX_CHECK" "has_sender=True"
e2e_assert_contains "body preserved" "$INBOX_CHECK" "has_body=True"
e2e_assert_contains "thread_id preserved" "$INBOX_CHECK" "has_thread=True"

# Acknowledge the message
PHASE3B_RESP="$(send_jsonrpc_session "$WF_DB" "$WF_STORAGE" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":32,\"method\":\"tools/call\",\"params\":{\"name\":\"acknowledge_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"BluePeak\",\"message_id\":${MSG_ID}}}}" \
)"
e2e_save_artifact "phase3_ack.txt" "$PHASE3B_RESP"

ACK_ERROR="$(is_error_result "$PHASE3B_RESP" 32)"
ACK_TEXT="$(extract_result "$PHASE3B_RESP" 32)"
if [ "$ACK_ERROR" = "true" ]; then
    e2e_fail "acknowledge_message returned error"
    echo "    text: $ACK_TEXT"
else
    e2e_pass "acknowledge_message succeeded"
fi

ACK_TS="$(parse_json_field "$ACK_TEXT" "acknowledged_at")"
if [ -n "$ACK_TS" ] && [ "$ACK_TS" != "null" ] && [ "$ACK_TS" != "" ]; then
    e2e_pass "acknowledge set acknowledged_at: $ACK_TS"
else
    e2e_fail "acknowledge response missing acknowledged_at"
fi

# Reply to the message
PHASE3C_RESP="$(send_jsonrpc_session "$WF_DB" "$WF_STORAGE" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":33,\"method\":\"tools/call\",\"params\":{\"name\":\"reply_message\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"message_id\":${MSG_ID},\"sender_name\":\"BluePeak\",\"body_md\":\"Looks great! Merging now.\"}}}" \
)"
e2e_save_artifact "phase3_reply.txt" "$PHASE3C_RESP"

REPLY_ERROR="$(is_error_result "$PHASE3C_RESP" 33)"
REPLY_TEXT="$(extract_result "$PHASE3C_RESP" 33)"
if [ "$REPLY_ERROR" = "true" ]; then
    e2e_fail "reply_message returned error"
    echo "    text: $REPLY_TEXT"
else
    e2e_pass "reply_message succeeded"
fi

# reply_message returns {deliveries: [{payload: {subject, thread_id, id, ...}}], count}
REPLY_SUBJ="$(parse_json_field "$REPLY_TEXT" "deliveries.0.payload.subject")"
REPLY_THREAD="$(parse_json_field "$REPLY_TEXT" "deliveries.0.payload.thread_id")"
e2e_assert_contains "reply has Re: prefix" "$REPLY_SUBJ" "Re:"
e2e_assert_eq "reply preserves thread_id" "FEAT-42" "$REPLY_THREAD"

REPLY_ID="$(parse_json_field "$REPLY_TEXT" "deliveries.0.payload.id")"
if [ -n "$REPLY_ID" ] && [ "$REPLY_ID" != "None" ]; then
    e2e_pass "reply returned message id: $REPLY_ID"
else
    e2e_fail "reply missing id"
fi

# ===========================================================================
# Phase 4: Resources — inbox and thread
# ===========================================================================
e2e_case_banner "Phase 4: resource://inbox + resource://thread"

# Read resource://inbox/BluePeak
PHASE4_RESP="$(send_jsonrpc_session "$WF_DB" "$WF_STORAGE" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":40,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://inbox/BluePeak?project=${EP_SLUG}&limit=10\"}}" \
    "{\"jsonrpc\":\"2.0\",\"id\":41,\"method\":\"resources/read\",\"params\":{\"uri\":\"resource://thread/FEAT-42?project=${EP_SLUG}&include_bodies=true\"}}" \
)"
e2e_save_artifact "phase4_resources.txt" "$PHASE4_RESP"
if [ "$(is_error_result "$PHASE4_RESP" 40)" = "true" ] || \
   [ "$(is_error_result "$PHASE4_RESP" 41)" = "true" ]; then
    e2e_fail "resource session did not complete successfully"
else
    e2e_pass "resource session completed successfully"
fi

# Parse inbox resource
INBOX_RES="$(echo "$PHASE4_RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 40 and 'result' in d:
            contents = d['result'].get('contents', [])
            if contents:
                text = contents[0].get('text', '')
                print(text)
                sys.exit(0)
    except Exception:
        pass
print('')
" 2>/dev/null)"

if [ -n "$INBOX_RES" ]; then
    e2e_pass "resource://inbox returned content"
else
    e2e_fail "resource://inbox returned empty"
fi

# Parse thread resource
THREAD_RES="$(echo "$PHASE4_RESP" | python3 -c "
import sys, json
for line in sys.stdin:
    line = line.strip()
    if not line: continue
    try:
        d = json.loads(line)
        if d.get('id') == 41 and 'result' in d:
            contents = d['result'].get('contents', [])
            if contents:
                text = contents[0].get('text', '')
                print(text)
                sys.exit(0)
    except Exception:
        pass
print('')
" 2>/dev/null)"

if [ -n "$THREAD_RES" ]; then
    e2e_pass "resource://thread returned content"
    # Verify thread has both messages (original + reply)
    THREAD_CHECK="$(echo "$THREAD_RES" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    msgs = d if isinstance(d, list) else d.get('messages', d.get('thread', []))
    if isinstance(msgs, list):
        count = len(msgs)
        senders = [m.get('from', m.get('from_agent', '')) for m in msgs if isinstance(m, dict)]
        print(f'count={count}|senders={\",\".join(senders)}')
    else:
        print(f'unexpected_type={type(msgs).__name__}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"
    e2e_save_artifact "phase4_thread_parsed.txt" "$THREAD_CHECK"
    e2e_assert_contains "thread has 2 messages" "$THREAD_CHECK" "count=2"
    e2e_assert_contains "thread has RedFox" "$THREAD_CHECK" "RedFox"
    e2e_assert_contains "thread has BluePeak" "$THREAD_CHECK" "BluePeak"
else
    e2e_fail "resource://thread returned empty"
fi

# ===========================================================================
# Phase 5: Release file reservations
# ===========================================================================
e2e_case_banner "Phase 5: release_file_reservations"

PHASE5_RESP="$(send_jsonrpc_session "$WF_DB" "$WF_STORAGE" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":50,\"method\":\"tools/call\",\"params\":{\"name\":\"release_file_reservations\",\"arguments\":{\"project_key\":\"${PROJECT_PATH}\",\"agent_name\":\"RedFox\",\"paths\":[\"src/lib.rs\",\"src/main.rs\"]}}}" \
)"
e2e_save_artifact "phase5_release.txt" "$PHASE5_RESP"

REL_ERROR="$(is_error_result "$PHASE5_RESP" 50)"
if [ "$REL_ERROR" = "true" ]; then
    REL_TEXT="$(extract_result "$PHASE5_RESP" 50)"
    e2e_fail "release_file_reservations returned error"
    echo "    text: $REL_TEXT"
else
    e2e_pass "release_file_reservations succeeded"
fi

# ===========================================================================
# Phase 6: Macro equivalents
# ===========================================================================
e2e_case_banner "Phase 6: macro_start_session + macro_file_reservation_cycle"

MACRO_DB="${WORK}/macro_workflow.sqlite3"
MACRO_STORAGE="${WORK}/macro_storage"
mkdir -p "$MACRO_STORAGE"

PHASE6_RESP="$(send_jsonrpc_session "$MACRO_DB" "$MACRO_STORAGE" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":60,\"method\":\"tools/call\",\"params\":{\"name\":\"macro_start_session\",\"arguments\":{\"human_key\":\"/tmp/e2e_macro_workflow_$$\",\"program\":\"e2e-test\",\"model\":\"test-model\",\"task_description\":\"happy path macro test\",\"inbox_limit\":5}}}" \
)"
e2e_save_artifact "phase6_macro_session.txt" "$PHASE6_RESP"

MACRO_ERROR="$(is_error_result "$PHASE6_RESP" 60)"
MACRO_TEXT="$(extract_result "$PHASE6_RESP" 60)"
if [ "$MACRO_ERROR" = "true" ]; then
    e2e_fail "macro_start_session returned error"
    echo "    text: $MACRO_TEXT"
else
    e2e_pass "macro_start_session succeeded"
fi

MACRO_CHECK="$(echo "$MACRO_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    has_project = 'project' in d
    has_agent = 'agent' in d
    has_inbox = 'inbox' in d
    agent_name = d.get('agent', {}).get('name', '')
    print(f'project={has_project}|agent={has_agent}|inbox={has_inbox}|name={agent_name}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_assert_contains "macro has project" "$MACRO_CHECK" "project=True"
e2e_assert_contains "macro has agent" "$MACRO_CHECK" "agent=True"
e2e_assert_contains "macro has inbox" "$MACRO_CHECK" "inbox=True"

# Extract agent name from macro response for reservation cycle
MACRO_AGENT="$(echo "$MACRO_CHECK" | sed -n 's/.*name=\([^|]*\).*/\1/p')"
if [ -z "$MACRO_AGENT" ]; then
    e2e_fail "macro_start_session did not return an agent name"
fi

# macro_file_reservation_cycle
PHASE6B_RESP="$(send_jsonrpc_session "$MACRO_DB" "$MACRO_STORAGE" \
    "$INIT_REQ" \
    "{\"jsonrpc\":\"2.0\",\"id\":61,\"method\":\"tools/call\",\"params\":{\"name\":\"macro_file_reservation_cycle\",\"arguments\":{\"project_key\":\"/tmp/e2e_macro_workflow_$$\",\"agent_name\":\"${MACRO_AGENT}\",\"paths\":[\"src/lib.rs\"],\"reason\":\"macro workflow test\",\"ttl_seconds\":3600,\"auto_release\":false}}}" \
)"
e2e_save_artifact "phase6_macro_reserve.txt" "$PHASE6B_RESP"

MCYCLE_ERROR="$(is_error_result "$PHASE6B_RESP" 61)"
MCYCLE_TEXT="$(extract_result "$PHASE6B_RESP" 61)"
if [ "$MCYCLE_ERROR" = "true" ]; then
    e2e_fail "macro_file_reservation_cycle returned error"
    echo "    text: $MCYCLE_TEXT"
else
    e2e_pass "macro_file_reservation_cycle succeeded"
fi

MCYCLE_CHECK="$(echo "$MCYCLE_TEXT" | python3 -c "
import sys, json
try:
    d = json.loads(sys.stdin.read())
    fr = d.get('file_reservations', d)
    granted = fr.get('granted', [])
    print(f'granted={len(granted)}')
except Exception as e:
    print(f'PARSE_ERROR: {e}')
" 2>/dev/null)"

e2e_assert_contains "macro reserved 1 path" "$MCYCLE_CHECK" "granted=1"

# ===========================================================================
# Phase 7: Verify DB state via CLI
# ===========================================================================
e2e_case_banner "Phase 7: CLI verification of DB state"

# Verify agents are in the DB
# Port zero has no listener: the CLI must read this fixture's mailbox, even
# when the worker has its own daemon on the usual port.
CLI_AGENTS_STATUS=0
CLI_AGENTS="$(DATABASE_URL="sqlite:///${WF_DB}" STORAGE_ROOT="${WF_STORAGE}" \
    HTTP_HOST=127.0.0.1 HTTP_PORT=0 AGENT_MAIL_URL=http://127.0.0.1:0/mcp/ \
    am agents list --project "${PROJECT_PATH}" --json \
    2>"${E2E_ARTIFACT_DIR}/phase7_cli_agents.stderr")" || CLI_AGENTS_STATUS=$?
e2e_save_artifact "phase7_cli_agents.txt" "$CLI_AGENTS"
e2e_save_artifact "phase7_cli_agents.exit" "$CLI_AGENTS_STATUS"

if [ "$CLI_AGENTS_STATUS" -eq 0 ] && [ -n "$CLI_AGENTS" ]; then
    e2e_pass "am agents list returned output"
    e2e_assert_contains "CLI shows RedFox" "$CLI_AGENTS" "RedFox"
    e2e_assert_contains "CLI shows BluePeak" "$CLI_AGENTS" "BluePeak"
else
    e2e_fail "required am agents list verification errored"
fi

# ===========================================================================
# Phase 8: Offline CLI reservations must be visible to the archive-backed guard
# ===========================================================================
e2e_case_banner "Phase 8: offline CLI reservation lifecycle + guard"

if python3 - "$WF_DB" "$WF_STORAGE" "$PROJECT_PATH" "$(command -v am)" \
    "$E2E_ARTIFACT_DIR" <<'PY'
import datetime
import hashlib
import json
import os
from pathlib import Path
import signal
import sqlite3
import subprocess
import sys
import time

db, storage, project, binary, artifacts = sys.argv[1:]
results = Path(artifacts) / "offline_cli"
results.mkdir()
Path(project).mkdir(exist_ok=True)
env = os.environ.copy()
env.update(DATABASE_URL="sqlite:///" + db, STORAGE_ROOT=storage,
           HTTP_HOST="127.0.0.1", HTTP_PORT="0", AGENT_MAIL_URL="http://127.0.0.1:0/mcp/",
           AM_INTERFACE_MODE="cli", AM_ATC_ENABLED="false", AM_ATC_WRITE_MODE="off",
           LLM_ENABLED="false", NOTIFICATIONS_ENABLED="false", TUI_ENABLED="false",
           NO_COLOR="1", RUST_LOG="error")

def interrupted(signum, frame):
    raise InterruptedError(f"offline CLI workflow interrupted by signal {signum}")

signal.signal(signal.SIGTERM, interrupted)

def run(name, args, expected=0, stdin="", agent="RedFox"):
    child_env = dict(env, AGENT_NAME=agent)
    started = time.monotonic()
    with open(binary, "rb") as executable:
        digest = hashlib.file_digest(executable, "sha256").hexdigest()
    proc = subprocess.Popen([binary, *args], cwd=results, env=child_env,
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE, text=True, start_new_session=True)
    failure = None
    try:
        stdout, stderr = proc.communicate(stdin, timeout=45)
    except BaseException as error:
        failure = error
        try:
            os.killpg(proc.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            stdout, stderr = proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(proc.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            stdout, stderr = proc.communicate(timeout=5)
    (results / (name + ".stdout")).write_text(stdout)
    (results / (name + ".stderr")).write_text(stderr)
    (results / (name + ".json")).write_text(json.dumps({
        "argv": args, "pid": proc.pid, "binary_sha256": digest,
        "exit_code": proc.returncode, "timed_out": isinstance(failure, subprocess.TimeoutExpired),
        "failure": None if failure is None else str(failure),
        "elapsed_s": time.monotonic() - started}, indent=2))
    if failure is not None:
        raise failure
    assert proc.returncode == expected, (name, proc.returncode, stderr)
    return stdout, stderr

def payload(stdout):
    return json.JSONDecoder().raw_decode(stdout[stdout.index("{"):])[0]

def row(reservation_id):
    with sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True) as conn:
        conn.row_factory = sqlite3.Row
        return dict(conn.execute(
            "SELECT id, path_pattern, exclusive, expires_ts, released_ts "
            "FROM file_reservations WHERE id = ?", (reservation_id,)).fetchone())

def artifact(reservation_id):
    matches = []
    for path in Path(storage).glob("projects/*/file_reservations/*.json"):
        value = json.loads(path.read_text())
        if value.get("id") == reservation_id:
            matches.append((path, value))
    stable = [value for path, value in matches if path.name.startswith("id-")]
    assert len(stable) == 1, (reservation_id, matches)
    for path, value in matches:
        assert value == stable[0], ("reservation aliases disagree", path, value, stable[0])
    return stable[0]

def micros(iso):
    dt = datetime.datetime.fromisoformat(iso.replace("Z", "+00:00"))
    delta = dt - datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc)
    return ((delta.days * 86400 + delta.seconds) * 1000000 + delta.microseconds)

pattern = "src/offline-cycle.rs"
stdout, _ = run("reserve", ["file_reservations", "reserve", project, "RedFox", pattern,
                            "--exclusive", "--ttl", "3600", "--reason", "br-21gj.4.4"])
grant = payload(stdout)
assert len(grant["granted"]) == 1 and not grant["conflicts"], grant
reservation_id = grant["granted"][0]["id"]
before = row(reservation_id)
active = artifact(reservation_id)
assert before["path_pattern"] == active["path_pattern"] == pattern
assert before["exclusive"] == 1 and active["exclusive"] is True
assert active["agent"] == "RedFox" and active["reason"] == "br-21gj.4.4"
assert before["released_ts"] is None and active.get("released_ts") is None
assert micros(active["expires_ts"]) == before["expires_ts"]
_, conflict = run("guard_active", ["guard", "check", "--repo", project],
                  expected=1, stdin=pattern + "\n", agent="BluePeak")
assert "CONFLICT" in conflict and "RedFox" in conflict, conflict
stdout, _ = run("conflict", ["file_reservations", "reserve", project, "BluePeak", pattern,
                             "--exclusive", "--ttl", "3600"])
denied = payload(stdout)
assert denied["granted"] == [] and len(denied["conflicts"]) == 1, denied
assert denied["conflicts"][0]["holders"][0]["agent"] == "RedFox", denied
with sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True) as conn:
    assert conn.execute(
        "SELECT COUNT(*) FROM file_reservations WHERE path_pattern = ? AND released_ts IS NULL",
        (pattern,)).fetchone()[0] == 1, "a conflict must not create another active lease"
run("renew", ["file_reservations", "renew", project, "RedFox",
              "--ids", str(reservation_id), "--extend-seconds", "1800"])
renewed = row(reservation_id)
assert renewed["expires_ts"] >= before["expires_ts"] + 1800 * 1000000
assert micros(artifact(reservation_id)["expires_ts"]) == renewed["expires_ts"]
run("release", ["file_reservations", "release", project, "RedFox",
                "--ids", str(reservation_id)])
released = row(reservation_id)
assert released["released_ts"] > 0
assert micros(artifact(reservation_id)["released_ts"]) == released["released_ts"]
stdout, _ = run("guard_released", ["guard", "check", "--repo", project],
                stdin=pattern + "\n", agent="BluePeak")
assert "No file reservation conflicts" in stdout, stdout
stdout, _ = run("release_again", ["file_reservations", "release", project, "RedFox",
                                 "--ids", str(reservation_id)])
assert "Released 0 reservation(s)" in stdout, stdout
assert row(reservation_id) == released
assert micros(artifact(reservation_id)["released_ts"]) == released["released_ts"]
print("Offline CLI reserve/conflict/renew/release and archive-backed guard passed.")
PY
then
    e2e_pass "offline CLI lifecycle preserves DB/archive parity and guard enforcement"
else
    e2e_fail "offline CLI reservation lifecycle or guard verification failed"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
