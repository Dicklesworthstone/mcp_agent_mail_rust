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
#   6. Offline CLI and macro reservations → archive-backed guard enforcement
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
from contextlib import closing
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
    with closing(sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True)) as conn:
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

session_project = str((results / "session_project").resolve())
Path(session_project).mkdir()
session_pattern = "src/session-start.rs"
stdout, _ = run("session_start", ["macros", "start-session", "--project", session_project,
                                 "--program", "codex", "--model", "workflow-fixture",
                                 "--agent-name", "RedFox", "--reserve", session_pattern,
                                 "--reserve-reason", "br-21gj.4.6", "--json"])
session = payload(stdout)
assert session["agent"]["name"] == "RedFox", session
assert session["file_reservations"]["conflicts"] == [], session
assert len(session["file_reservations"]["granted"]) == 1, session
session_id = session["file_reservations"]["granted"][0]["id"]
session_row = row(session_id)
session_artifact = artifact(session_id)
assert session_row["path_pattern"] == session_artifact["path_pattern"] == session_pattern
assert session_row["exclusive"] == 1 and session_artifact["exclusive"] is True
assert session_row["released_ts"] is None and session_artifact.get("released_ts") is None
assert micros(session_artifact["expires_ts"]) == session_row["expires_ts"]
profile = Path(storage) / "projects" / session["project"]["slug"] / "agents" / "RedFox" / "profile.json"
assert json.loads(profile.read_text())["name"] == "RedFox", profile
_, conflict = run("session_guard_held", ["guard", "check", "--repo", session_project],
                  expected=1, stdin=session_pattern + "\n", agent="BluePeak")
assert "CONFLICT" in conflict and "RedFox" in conflict, conflict
run("session_release", ["file_reservations", "release", session_project, "RedFox",
                        "--ids", str(session_id)])
assert row(session_id)["released_ts"] > 0
assert micros(artifact(session_id)["released_ts"]) == row(session_id)["released_ts"]
stdout, _ = run("session_guard_released", ["guard", "check", "--repo", session_project],
                stdin=session_pattern + "\n", agent="BluePeak")
assert "No file reservation conflicts" in stdout, stdout

run("session_target", ["macros", "start-session", "--project", session_project,
                       "--program", "codex", "--model", "workflow-fixture",
                       "--agent-name", "BluePeak", "--json"], agent="BluePeak")
stdout, _ = run("contact_implicit_requester", ["macros", "contact-handshake",
                "--project", session_project, "--from", "GreenLake", "--to", "BluePeak",
                "--register-missing", "--reg-program", "codex", "--reg-model", "workflow-fixture",
                "--reg-task", "implicit requester archive parity", "--auto-accept", "--json"],
                agent="GreenLake")
implicit_contact = payload(stdout)
assert implicit_contact["response"]["approved"] is True, implicit_contact
assert implicit_contact["response"]["updated"] == 1, implicit_contact
implicit_profile_path = profile.parent.parent / "GreenLake" / "profile.json"
implicit_profile = json.loads(implicit_profile_path.read_text())
with closing(sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True)) as conn:
    implicit_row = conn.execute(
        "SELECT a.name, a.program, a.model, a.task_description FROM agents a "
        "JOIN projects p ON p.id = a.project_id WHERE p.human_key = ? AND a.name = 'GreenLake'",
        (session_project,)).fetchone()
    assert implicit_row == ("GreenLake", "codex", "workflow-fixture",
                            "implicit requester archive parity"), implicit_row
    assert conn.execute(
        "SELECT l.status FROM agent_links l JOIN agents a ON a.id = l.a_agent_id "
        "JOIN agents b ON b.id = l.b_agent_id JOIN projects p ON p.id = l.a_project_id "
        "WHERE p.human_key = ? AND a.name = 'GreenLake' AND b.name = 'BluePeak'",
        (session_project,)).fetchall() == [("approved",)]
assert tuple(implicit_profile[field] for field in ("name", "program", "model", "task_description")) == implicit_row
assert "registration_token" not in implicit_profile, implicit_profile_path
welcome_body = "Retain this synthetic CLI macro welcome in SQLite and the Git archive."
macro_thread = "kp1in-offline-macro-thread"
stdout, _ = run("contact_welcome", ["macros", "contact-handshake", "--project", session_project,
                                  "--from", "RedFox", "--to", "BluePeak", "--auto-accept",
                                  "--welcome-subject", "Offline macro welcome",
                                  "--welcome-body", welcome_body, "--thread-id", macro_thread,
                                  "--json"])
handshake = payload(stdout)
assert handshake["response"]["approved"] is True, handshake
assert handshake["response"]["updated"] == 1, handshake
welcome_id = handshake["welcome_message"]["deliveries"][0]["payload"]["id"]
with closing(sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True)) as conn:
    assert conn.execute(
        "SELECT l.status FROM agent_links l "
        "JOIN agents a ON a.id = l.a_agent_id JOIN agents b ON b.id = l.b_agent_id "
        "JOIN projects p ON p.id = l.a_project_id "
        "WHERE p.human_key = ? AND a.name = 'RedFox' AND b.name = 'BluePeak'",
        (session_project,)).fetchall() == [("approved",)]
    assert conn.execute("SELECT body_md, thread_id FROM messages WHERE id = ?",
                        (welcome_id,)).fetchone() == (welcome_body, macro_thread)
welcome_archives = []
for path in Path(storage).glob("projects/*/messages/**/*.md"):
    content = path.read_text()
    if content.startswith("---json\n") and "\n---\n" in content:
        metadata, archived_body = content[8:].split("\n---\n", 1)
        if json.loads(metadata).get("id") == welcome_id:
            welcome_archives.append(path)
            assert archived_body.strip() == welcome_body
assert len(welcome_archives) == 1, welcome_archives
stdout, _ = run("prepare_thread", ["macros", "prepare-thread", "--project", session_project,
                                  "--thread-id", macro_thread, "--program", "codex",
                                  "--model", "workflow-fixture", "--agent-name", "GreenLake",
                                  "--json"], agent="GreenLake")
prepared = payload(stdout)
assert prepared["agent"]["name"] == "GreenLake", prepared
assert prepared["thread"]["total_messages"] == 1, prepared
assert prepared["thread"]["summary"]["participants"] == ["RedFox"], prepared
assert [example["id"] for example in prepared["thread"]["examples"]] == [welcome_id], prepared
prepared_profile = profile.parent.parent / "GreenLake" / "profile.json"
assert json.loads(prepared_profile.read_text())["name"] == "GreenLake", prepared_profile
stdout, _ = run("prepare_existing", ["macros", "prepare-thread", "--project", session_project,
                                    "--thread-id", macro_thread, "--program", "codex",
                                    "--model", "workflow-fixture", "--agent-name", "BluePeak",
                                    "--no-register", "--no-examples", "--inbox-bodies", "--json"],
                agent="BluePeak")
existing = payload(stdout)
assert existing["thread"]["examples"] == [], existing
assert any(message["id"] == welcome_id and message["body_md"] == welcome_body
           for message in existing["inbox"]), existing

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
with closing(sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True)) as conn:
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

macro_pattern = "src/offline-macro-cycle.rs"
stdout, _ = run("macro_reserve", ["macros", "file-reservation-cycle", "-p", project,
                                "-a", "RedFox", "--path", macro_pattern,
                                "--reason", "br-21gj.4.6", "--json"])
macro = payload(stdout)
assert macro["released"] is None and macro["file_reservations"]["conflicts"] == [], macro
assert len(macro["file_reservations"]["granted"]) == 1, macro
macro_id = macro["file_reservations"]["granted"][0]["id"]
assert row(macro_id)["released_ts"] is None and row(macro_id)["exclusive"] == 1
assert artifact(macro_id)["agent"] == "RedFox"
assert artifact(macro_id)["path_pattern"] == macro_pattern
assert artifact(macro_id).get("released_ts") is None
stdout, stderr = run("macro_guard_held", ["guard", "check", "--repo", project],
                     expected=1, stdin=macro_pattern + "\n", agent="BluePeak")
assert "RedFox" in stdout + stderr, (stdout, stderr)
stdout, _ = run("macro_conflict", ["macros", "file-reservation-cycle", "-p", project,
                                 "-a", "BluePeak", "--path", macro_pattern, "--json"])
conflict = payload(stdout)["file_reservations"]
assert conflict["granted"] == [] and len(conflict["conflicts"]) == 1, conflict
assert conflict["conflicts"][0]["holders"][0]["agent"] == "RedFox", conflict
with closing(sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True)) as conn:
    assert conn.execute("SELECT COUNT(*) FROM file_reservations WHERE path_pattern = ? "
                        "AND released_ts IS NULL", (macro_pattern,)).fetchone()[0] == 1
run("macro_cleanup_release", ["file_reservations", "release", project, "RedFox",
                              "--ids", str(macro_id)])
assert row(macro_id)["released_ts"] > 0
assert micros(artifact(macro_id)["released_ts"]) == row(macro_id)["released_ts"]

auto_pattern = "src/offline-macro-auto.rs"
stdout, _ = run("macro_auto_release", ["macros", "file-reservation-cycle", "-p", project,
                                     "-a", "RedFox", "--path", auto_pattern,
                                     "--auto-release", "--json"])
auto = payload(stdout)
assert len(auto["file_reservations"]["granted"]) == 1, auto
assert auto["released"]["released"] == 1 and not auto["released"].get("queued", False), auto
auto_id = auto["file_reservations"]["granted"][0]["id"]
auto_row = row(auto_id)
assert auto_row["released_ts"] > 0
assert micros(artifact(auto_id)["released_ts"]) == auto_row["released_ts"]
stdout, _ = run("macro_guard_released", ["guard", "check", "--repo", project],
                stdin=auto_pattern + "\n", agent="BluePeak")
assert "No file reservation conflicts" in stdout, stdout
print("Offline CLI and macro reservation lifecycle and archive-backed guard passed.")
PY
then
    e2e_pass "offline CLI and macro lifecycle preserve DB/archive parity and guard enforcement"
else
    e2e_fail "offline CLI or macro reservation lifecycle or guard verification failed"
fi

# ===========================================================================
# Phase 9: HTTP handoff, signal drain, and persisted mailbox reopen
# ===========================================================================
e2e_case_banner "Phase 9: HTTP messaging + clean signal shutdown + reopen"

if python3 - "$WF_DB" "$WF_STORAGE" "$PROJECT_PATH" "$(command -v am)" \
    "$E2E_ARTIFACT_DIR" <<'PY'
from contextlib import closing
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import sqlite3
import selectors
import subprocess
import sys
import time
import urllib.request

db, storage, project, binary, artifacts = sys.argv[1:]
run = Path(artifacts) / "http_handoff"
run.mkdir(mode=0o700, exist_ok=False)
summary = {"passed": False, "client_pid": os.getpid(), "servers": [], "completed": []}
server = None
started = time.monotonic()

def interrupted(signum, frame):
    raise InterruptedError(f"HTTP handoff interrupted by signal {signum}")

signal.signal(signal.SIGTERM, interrupted)

def start(label):
    global server, endpoint, headers
    with socket.socket() as available:
        available.bind(("127.0.0.1", 0))
        port = available.getsockname()[1]
    env = os.environ.copy()
    env.pop("AM_INTERFACE_MODE", None)
    env.update(DATABASE_URL="sqlite:///" + db, STORAGE_ROOT=storage,
               HTTP_HOST="127.0.0.1", HTTP_PORT=str(port),
               HTTP_BEARER_TOKEN="owned-http-handoff-fixture", HTTP_JWT_ENABLED="false",
               HTTP_RATE_LIMIT_ENABLED="false", INVOCATION_ID="kp1in-handoff-fixture",
               AM_ATC_ENABLED="false", AM_ATC_WRITE_MODE="off", ATC_LEARNING_DISABLED="1",
               WORKTREES_ENABLED="true",
               LLM_ENABLED="false", NOTIFICATIONS_ENABLED="false", TUI_ENABLED="false",
               MCP_AGENT_MAIL_OUTPUT_FORMAT="json", TOON_DEFAULT_FORMAT="", RUST_LOG="warn")
    with (run / (label + ".stdout")).open("xb") as stdout, (run / (label + ".stderr")).open("xb") as stderr:
        server = subprocess.Popen([binary, "serve-http", "--host", "127.0.0.1",
                                   "--port", str(port), "--no-tui"], cwd=run, env=env,
                                  stdout=stdout, stderr=stderr, start_new_session=True)
    summary["servers"].append({"label": label, "pid": server.pid, "port": port})
    endpoint = f"http://127.0.0.1:{port}/mcp/"
    headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
               "Authorization": "Bearer owned-http-handoff-fixture"}
    deadline = time.monotonic() + 30
    while True:
        assert server.poll() is None, (label, "server exited before listening", server.returncode)
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.2):
                break
        except OSError:
            if time.monotonic() >= deadline:
                raise TimeoutError(f"{label}: HTTP bind deadline")
            time.sleep(0.1)

def rpc(label, method, params, request_id):
    request = {"jsonrpc": "2.0", "method": method, "params": params}
    if request_id is not None:
        request["id"] = request_id
    (run / (label + ".request.json")).write_text(json.dumps(request))
    with urllib.request.urlopen(urllib.request.Request(endpoint, data=json.dumps(request).encode(),
                                 headers=headers), timeout=35) as response:
        raw = response.read(8 * 1024 * 1024 + 1)
        assert len(raw) <= 8 * 1024 * 1024, "HTTP response budget exceeded"
        if response.headers.get("Mcp-Session-Id"):
            headers["Mcp-Session-Id"] = response.headers["Mcp-Session-Id"]
    (run / (label + ".response.json")).write_bytes(raw)
    if request_id is None:
        return None
    decoded = json.loads(raw)
    assert decoded.get("id") == request_id and "error" not in decoded, decoded
    result = decoded["result"]
    assert not result.get("isError"), result
    summary["completed"].append(label)
    return result

def initialize(label):
    rpc(label + "_initialize", "initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
        "clientInfo": {"name": "http-handoff-e2e", "version": "1"}}, 1)
    rpc(label + "_initialized", "notifications/initialized", {}, None)

def tool(label, name, arguments, request_id):
    result = rpc(label, "tools/call", {"name": name, "arguments": dict(arguments, format="json")}, request_id)
    assert len(result["content"]) == 1, result
    return json.loads(result["content"][0]["text"])

def stop(signum):
    global server
    assert server.poll() is None, "server exited before its requested stop"
    summary["servers"][-1]["requested_signal"] = signum
    os.killpg(server.pid, signum)
    code = server.wait(timeout=75)
    summary["servers"][-1]["exit"] = code
    assert code == 0, ("headless stop must drain and return success", signum, code)
    server = None

def recipient_cursors():
    actors = ["RedFox", "BluePeak", "GreenLake", "GoldHawk", "SilverWolf"]
    roles = {"BluePeak": "to", "GreenLake": "cc", "GoldHawk": "bcc"}
    for index, actor in enumerate(actors[3:]):
        tool("cursor_register_" + actor, "register_agent", {
            "project_key": project, "program": "codex", "model": "workflow-fixture",
            "name": actor}, 200 + index)
        tool("cursor_policy_" + actor, "set_contact_policy", {
            "project_key": project, "agent_name": actor, "policy": "open"}, 210 + index)
    before = {}
    for index, actor in enumerate(actors):
        position = tool("cursor_before_" + actor, "fetch_inbox_events", {
            "project_key": project, "agent_name": actor, "position_now": True}, 220 + index)
        assert position["events"] == [] and position["has_more"] is False, position
        assert position["next_cursor"] == position["tail_cursor"], position
        before[actor] = position["next_cursor"]
    body = "Synthetic delivery for explicit to, cc and bcc recipient checks."
    sent = tool("cursor_send", "send_message", {"project_key": project,
        "sender_name": "RedFox", "to": ["BluePeak"], "cc": ["GreenLake"], "bcc": ["GoldHawk"],
        "subject": "Cursor recipient isolation", "body_md": body, "ack_required": True,
        "thread_id": "kp1in-recipient-cursors", "topic": "Kp1in-Cursors",
        "idempotency_key": "kp1in-recipient-cursors"}, 230)
    payload = sent["deliveries"][0]["payload"]
    assert payload["to"] == ["BluePeak"] and payload["cc"] == ["GreenLake"], payload
    assert payload["bcc"] == ["GoldHawk"], payload
    message_id = payload["id"]
    positions = {}
    for index, actor in enumerate(actors):
        page = tool("cursor_events_" + actor, "fetch_inbox_events", {
            "project_key": project, "agent_name": actor, "after": before[actor], "limit": 1}, 240 + index)
        events = page["events"]
        assert page["has_more"] is False, page
        if actor in roles:
            assert len(events) == 1, (actor, page)
            event = events[0]
            assert event["message_id"] == message_id and event["kind"] == roles[actor], event
            assert event["cursor"] > before[actor] and event["cursor"] == page["next_cursor"], page
            assert event["from"] == "RedFox" and event["ack_required"] is True, event
            assert "body_md" not in event and "bcc" not in event, event
        else:
            assert events == [], ("nonrecipient received delivery", actor, page)
        positions[actor] = page["next_cursor"]
    # Cursor reads must not mark the real recipient rows read or acknowledged.
    with closing(sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True)) as conn:
        rows = conn.execute("SELECT kind, read_ts, ack_ts FROM message_recipients WHERE message_id = ?",
                            (message_id,)).fetchall()
        assert sorted(rows) == [(kind, None, None) for kind in sorted(roles.values())], rows
    for index, actor in enumerate(actors):
        inbox = tool("cursor_inbox_" + actor, "fetch_inbox", {
            "project_key": project, "agent_name": actor, "include_bodies": True,
            "topic": "KP1IN-CURSORS", "limit": 100}, 250 + index)
        assert len(inbox) == (1 if actor in roles else 0), (actor, inbox)
        if actor in roles:
            message = inbox[0]
            assert message["id"] == message_id and message["body_md"] == body, message
            assert message["kind"] == roles[actor], message
            assert message.get("bcc", []) == [], ("BCC identity exposed to a recipient", actor, message)
            ack = tool("cursor_ack_" + actor, "acknowledge_message", {
                "project_key": project, "agent_name": actor, "message_id": message_id}, 260 + index)
            assert ack["acknowledged"] is True and ack["acknowledged_at"], ack
    # fetch_topic is intentionally project-scoped; it has no recipient filter.
    topic = tool("cursor_topic", "fetch_topic", {"project_key": project,
        "topic_name": "kp1in-cursors", "include_bodies": True}, 270)
    assert len(topic) == 1 and topic[0]["id"] == message_id and topic[0]["body_md"] == body, topic
    summary["recipient_cursors"] = {"message_id": message_id, "before": before,
        "after": positions, "roles": roles, "body_sha256": hashlib.sha256(body.encode()).hexdigest()}
    return message_id, positions, roles, body

def product_and_slots():
    product_row = tool("product_create", "ensure_product", {"name": "kp1in-workflow-product"}, 300)
    product_key = product_row["product_uid"]
    assert tool("product_repeat", "ensure_product", {"product_key": product_key}, 301) == product_row
    projects = [project, str(run / "linked-project"), str(run / "unlinked-project")]
    project_ids, message_ids = [], []
    for index, project_key in enumerate(projects):
        base = 310 + index * 10
        info = tool(f"product_project_{index}", "ensure_project", {"human_key": project_key}, base)
        project_ids.append(info["id"])
        if index:
            for offset, actor in enumerate(["RedFox", "BluePeak"]):
                tool(f"product_register_{index}_{actor}", "register_agent", {
                    "project_key": project_key, "program": "codex", "model": "workflow-fixture",
                    "name": actor}, base + 1 + offset)
            tool(f"product_policy_{index}", "set_contact_policy", {
                "project_key": project_key, "agent_name": "BluePeak", "policy": "open"}, base + 3)
        if index < 2:
            args = {"product_key": product_key, "project_key": project_key}
            linked = tool(f"product_link_{index}", "products_link", args, base + 4)
            assert linked["linked"] is True and linked["project"]["id"] == info["id"], linked
            assert tool(f"product_link_repeat_{index}", "products_link", args, base + 5) == linked
        sent = tool(f"product_send_{index}", "send_message", {
            "project_key": project_key, "sender_name": "RedFox", "to": ["BluePeak"],
            "subject": "Product scope fixture", "body_md": f"Synthetic product scope {index}",
            "thread_id": "kp1in-product-scope", "idempotency_key": "kp1in-product-scope"}, base + 6)
        message_ids.append(sent["deliveries"][0]["payload"]["id"])
    inbox = tool("product_inbox", "fetch_inbox_product", {"product_key": product_key,
        "agent_name": "BluePeak", "include_bodies": True, "limit": 100}, 350)
    selected = [row for row in inbox if row.get("thread_id") == "kp1in-product-scope"]
    assert sorted((row["project_id"], row["id"], row["body_md"]) for row in selected) == sorted(
        (project_ids[index], message_ids[index], f"Synthetic product scope {index}") for index in range(2)), selected
    assert message_ids[2] not in [row["id"] for row in inbox], inbox
    with closing(sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True)) as conn:
        links = conn.execute("SELECT project_id FROM product_project_links WHERE product_id = ?",
                             (product_row["id"],)).fetchall()
        assert sorted(row[0] for row in links) == sorted(project_ids[:2]), links
        for message_id in message_ids:
            assert conn.execute("SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = ?",
                                (message_id,)).fetchall() == [(None, None)]
    slot = {"project_key": project, "slot": "kp1in-advisory-slot"}
    first = tool("slot_first", "acquire_build_slot", dict(slot, agent_name="RedFox",
        ttl_seconds=120, exclusive=True), 360)
    assert first["conflicts"] == [] and first["granted"]["agent"] == "RedFox", first
    second = tool("slot_conflict", "acquire_build_slot", dict(slot, agent_name="BluePeak",
        ttl_seconds=120, exclusive=False), 361)
    # A conflict is advisory: the second lease is still granted and persisted.
    assert second["granted"]["agent"] == "BluePeak" and second["granted"]["exclusive"] is False, second
    assert [row["agent"] for row in second["conflicts"]] == ["RedFox"], second
    renewed = tool("slot_renew", "renew_build_slot", dict(slot, agent_name="RedFox", extend_seconds=300), 362)
    assert renewed["renewed"] is True and renewed["expires_ts"] > first["granted"]["expires_ts"], renewed
    leases = [json.loads(path.read_text()) for path in Path(storage).glob(
        "projects/*/build_slots/kp1in-advisory-slot/*.json")]
    assert sorted(row["agent"] for row in leases) == ["BluePeak", "RedFox"], leases
    assert all(not row.get("released_ts") for row in leases), leases
    assert next(row for row in leases if row["agent"] == "RedFox")["expires_ts"] == renewed["expires_ts"]
    summary["product_scope"] = {"product": product_row, "projects": project_ids, "messages": message_ids}
    summary["advisory_slots"] = {"slot": slot, "renewed_expires_ts": renewed["expires_ts"]}

def concurrent_ring():
    # Each client is a separate OS process with its own MCP session. The two
    # parent barriers ensure all identities exist before sends and all sends
    # have acknowledged commits before recipients inspect their inboxes.
    client_source = r'''
import hashlib
import json
import os
from pathlib import Path
import sys
import time
import urllib.request

endpoint, project, actor, recipient, incoming_from, directory = sys.argv[1:]
headers = {"Content-Type": "application/json",
           "Accept": "application/json, text/event-stream",
           "Authorization": "Bearer owned-http-handoff-fixture"}
counter = 0
history = (Path(directory) / "history.jsonl").open("x", buffering=1)

def rpc(method, params, notification=False):
    global counter
    counter += 1
    request = {"jsonrpc": "2.0", "method": method, "params": params}
    if not notification:
        request["id"] = counter
    raw_request = json.dumps(request, sort_keys=True).encode()
    history.write(json.dumps({"event": "invoke", "id": counter, "method": method,
        "client_pid": os.getpid(), "monotonic_ns": time.monotonic_ns(),
        "sha256": hashlib.sha256(raw_request).hexdigest()}) + "\n")
    with urllib.request.urlopen(urllib.request.Request(endpoint, data=raw_request,
                                 headers=headers), timeout=25) as response:
        raw = response.read(8 * 1024 * 1024 + 1)
        assert len(raw) <= 8 * 1024 * 1024, "client response budget exceeded"
        if response.headers.get("Mcp-Session-Id"):
            headers["Mcp-Session-Id"] = response.headers["Mcp-Session-Id"]
    history.write(json.dumps({"event": "complete", "id": counter,
        "client_pid": os.getpid(), "monotonic_ns": time.monotonic_ns(),
        "sha256": hashlib.sha256(raw).hexdigest()}) + "\n")
    if notification:
        return None
    decoded = json.loads(raw)
    assert decoded.get("id") == counter and "error" not in decoded, decoded
    result = decoded["result"]
    assert not result.get("isError"), result
    return result

def tool(name, arguments):
    result = rpc("tools/call", {"name": name, "arguments": dict(arguments, format="json")})
    assert len(result["content"]) == 1, result
    return json.loads(result["content"][0]["text"])

def barrier(stage, **values):
    print(json.dumps(dict(stage=stage, actor=actor, pid=os.getpid(), **values)), flush=True)
    if stage != "done":
        assert sys.stdin.readline().strip() == "go", "parent barrier ended"

try:
    rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
        "clientInfo": {"name": "concurrent-ring-" + actor, "version": "1"}})
    rpc("notifications/initialized", {}, notification=True)
    identity = tool("register_agent", {"project_key": project, "program": "codex",
        "model": "workflow-fixture", "name": actor})
    tool("set_contact_policy", {"project_key": project, "agent_name": actor, "policy": "open"})
    barrier("ready")
    arguments = {"project_key": project, "sender_name": actor, "to": [recipient],
        "subject": "Concurrent ring " + actor, "body_md": "Durable concurrent message from " + actor,
        "thread_id": "kp1in-concurrent-ring", "ack_required": True,
        "sender_token": identity["registration_token"], "idempotency_key": "kp1in-ring-" + actor}
    sent = tool("send_message", arguments)
    sent_id = sent["deliveries"][0]["payload"]["id"]
    assert sent["verified_sender"] is True, sent
    replay = tool("send_message", arguments)
    assert replay["deliveries"][0]["payload"]["id"] == sent_id, replay
    assert replay["idempotent_replay"] is True, replay
    barrier("sent", message_id=sent_id)
    inbox = tool("fetch_inbox", {"project_key": project, "agent_name": actor,
                                "include_bodies": True, "limit": 100})
    received = [row for row in inbox if row.get("thread_id") == "kp1in-concurrent-ring"]
    assert len(received) == 1, received
    message = received[0]
    assert message["from"] == incoming_from, message
    assert message["body_md"] == "Durable concurrent message from " + incoming_from, message
    ack_args = {"project_key": project, "agent_name": actor, "message_id": message["id"]}
    first_ack = tool("acknowledge_message", ack_args)
    second_ack = tool("acknowledge_message", ack_args)
    assert first_ack["acknowledged"] is True and first_ack["acknowledged_at"], first_ack
    assert second_ack["acknowledged"] is True and second_ack["acknowledged_at"], second_ack
    barrier("done", sent_id=sent_id, received_id=message["id"], recipient=recipient,
            incoming_from=incoming_from, acknowledged_at=second_ack["acknowledged_at"])
finally:
    history.close()
'''
    client_file = run / "concurrent_client.py"
    with client_file.open("x") as script:
        script.write(client_source)
    names = ["RedFox", "BluePeak", "GreenLake"]
    clients = []
    buffers = {}
    selector = selectors.DefaultSelector()
    deadline = time.monotonic() + 120
    records = []
    summary["concurrent_clients"] = records

    def stage(expected):
        pending = set(range(len(clients)))
        values = {}
        while pending:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"concurrent client barrier {expected}: {sorted(pending)}")
            for key, _ in selector.select(min(remaining, 0.5)):
                index = key.data
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    raise RuntimeError(f"client {names[index]} EOF before barrier {expected}")
                buffers[index] += chunk
                assert len(buffers[index]) <= 65536, "client status budget exceeded"
                while b"\n" in buffers[index]:
                    line, buffers[index] = buffers[index].split(b"\n", 1)
                    value = json.loads(line)
                    assert index in pending and value["stage"] == expected, value
                    assert value["pid"] == clients[index].pid and value["actor"] == names[index], value
                    values[names[index]] = value
                    pending.remove(index)
                if index not in pending:
                    selector.unregister(key.fileobj)
        return values

    def advance():
        for index, child in enumerate(clients):
            child.stdin.write(b"go\n")
            child.stdin.flush()
            selector.register(child.stdout, selectors.EVENT_READ, index)

    try:
        for index, name in enumerate(names):
            directory = run / ("client-" + name)
            directory.mkdir(mode=0o700)
            with (directory / "stderr.txt").open("xb") as stderr:
                child = subprocess.Popen([sys.executable, str(client_file), endpoint, project, name,
                    names[(index + 1) % len(names)], names[(index - 1) % len(names)], str(directory)],
                    stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=stderr,
                    start_new_session=True)
            clients.append(child)
            records.append({"actor": name, "pid": child.pid, "history": str(directory / "history.jsonl")})
            buffers[index] = b""
            selector.register(child.stdout, selectors.EVENT_READ, index)
        assert len({child.pid for child in clients}) == 3
        stage("ready")
        summary["concurrent_ready_barrier_ns"] = time.monotonic_ns()
        advance()
        sent = stage("sent")
        assert len({value["message_id"] for value in sent.values()}) == 3, sent
        advance()
        results = stage("done")
        for record, child in zip(records, clients):
            record["exit"] = child.wait(timeout=max(0.1, deadline - time.monotonic()))
            assert record["exit"] == 0, record
            record["result"] = results[record["actor"]]
        for value in results.values():
            assert value["received_id"] == results[value["incoming_from"]]["sent_id"], results
        return results
    finally:
        selector.close()
        for record, child in zip(records, clients):
            if child.poll() is None:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait(timeout=5)
                    record["forced_cleanup"] = True
            record["exit"] = child.returncode
            child.stdin.close()
            child.stdout.close()

try:
    with open(binary, "rb") as executable:
        summary["binary_sha256"] = hashlib.file_digest(executable, "sha256").hexdigest()
    start("first")
    initialize("first")
    body = "Persist this synthetic HTTP handoff message through signal drain and restart."
    sent = tool("send", "send_message", {"project_key": project, "sender_name": "RedFox",
        "to": ["BluePeak"], "subject": "HTTP handoff persistence", "body_md": body,
        "ack_required": True}, 2)
    message_id = sent["deliveries"][0]["payload"]["id"]
    summary["message_id"] = message_id
    arguments = {"project_key": project, "agent_name": "BluePeak", "include_bodies": True}
    inbox = tool("inbox", "fetch_inbox", arguments, 3)
    assert any(row["id"] == message_id and row["body_md"] == body for row in inbox), inbox
    ack = tool("ack", "acknowledge_message", {"project_key": project, "agent_name": "BluePeak",
               "message_id": message_id}, 4)
    assert ack["acknowledged"] is True and ack["acknowledged_at"], ack
    ring = concurrent_ring()
    product_and_slots()
    cursor_message_id, cursor_positions, cursor_roles, cursor_body = recipient_cursors()
    stop(signal.SIGTERM)

    with closing(sqlite3.connect(Path(db).as_uri() + "?mode=ro", uri=True)) as conn:
        assert conn.execute("SELECT body_md FROM messages WHERE id = ?", (message_id,)).fetchone() == (body,)
        rows = conn.execute("SELECT read_ts, ack_ts FROM message_recipients WHERE message_id = ?",
                            (message_id,)).fetchall()
        assert len(rows) == 1 and all(value is not None and value > 0 for value in rows[0]), rows
        ring_rows = conn.execute(
            "SELECT m.id, a.name, m.body_md FROM messages m JOIN agents a ON a.id = m.sender_id "
            "WHERE m.thread_id = 'kp1in-concurrent-ring'").fetchall()
        expected_ring = sorted((value["sent_id"], actor, "Durable concurrent message from " + actor)
                               for actor, value in ring.items())
        assert sorted(ring_rows) == expected_ring, ring_rows
        recipients = conn.execute(
            "SELECT r.message_id, a.name, r.read_ts, r.ack_ts FROM message_recipients r "
            "JOIN messages m ON m.id = r.message_id JOIN agents a ON a.id = r.agent_id "
            "WHERE m.thread_id = 'kp1in-concurrent-ring'").fetchall()
        assert len(recipients) == 3, recipients
        assert sorted((row[0], row[1]) for row in recipients) == sorted(
            (value["sent_id"], value["recipient"]) for value in ring.values()), recipients
        assert all(row[2] and row[3] for row in recipients), recipients
        cursor_rows = conn.execute(
            "SELECT a.name, r.kind, r.read_ts, r.ack_ts FROM message_recipients r "
            "JOIN agents a ON a.id = r.agent_id WHERE r.message_id = ?", (cursor_message_id,)).fetchall()
        assert sorted((row[0], row[1]) for row in cursor_rows) == sorted(cursor_roles.items()), cursor_rows
        assert all(row[2] and row[3] for row in cursor_rows), cursor_rows
        assert conn.execute("PRAGMA integrity_check").fetchall() == [("ok",)]
        assert conn.execute("PRAGMA foreign_key_check").fetchall() == []
    archived = []
    ring_archived = []
    cursor_archived = []
    for path in Path(storage).glob("projects/*/messages/**/*.md"):
        content = path.read_text()
        if content.startswith("---json\n") and "\n---\n" in content:
            metadata, archived_body = content[8:].split("\n---\n", 1)
            if json.loads(metadata).get("id") == message_id:
                archived.append(path)
                assert archived_body.strip() == body
            if json.loads(metadata).get("thread_id") == "kp1in-concurrent-ring":
                ring_archived.append((json.loads(metadata)["id"], archived_body.strip()))
            if json.loads(metadata).get("id") == cursor_message_id:
                assert archived_body.strip() == cursor_body
                cursor_archived.append(path)
    assert len(archived) == 1, archived
    assert sorted(ring_archived) == sorted((row[0], row[2]) for row in expected_ring), ring_archived
    assert len(cursor_archived) == 1, cursor_archived
    summary["archive_message"] = str(archived[0])

    start("reopen")
    initialize("reopen")
    inbox = tool("reopen_inbox", "fetch_inbox", arguments, 2)
    assert any(row["id"] == message_id and row["body_md"] == body for row in inbox), inbox
    for index, (actor, value) in enumerate(ring.items()):
        inbox = tool("reopen_ring_" + actor, "fetch_inbox",
                     {"project_key": project, "agent_name": value["recipient"],
                      "include_bodies": True, "limit": 100}, 10 + index)
        matches = [row for row in inbox if row.get("thread_id") == "kp1in-concurrent-ring"]
        assert len(matches) == 1 and matches[0]["id"] == value["sent_id"], matches
        assert matches[0]["body_md"] == "Durable concurrent message from " + actor, matches
    for index, (actor, cursor) in enumerate(cursor_positions.items()):
        page = tool("reopen_cursor_" + actor, "fetch_inbox_events", {
            "project_key": project, "agent_name": actor, "after": cursor, "limit": 1}, 30 + index)
        assert page["events"] == [] and page["has_more"] is False, (actor, page)
        assert page["next_cursor"] == cursor, (actor, page)
        # Rewinding one processed event replays the same durable delivery,
        # even though fetch_inbox and acknowledgement have changed read state.
        if actor in cursor_roles:
            replay = tool("reopen_cursor_replay_" + actor, "fetch_inbox_events", {
                "project_key": project, "agent_name": actor,
                "after": summary["recipient_cursors"]["before"][actor], "limit": 1}, 40 + index)
            assert len(replay["events"]) == 1 and replay["events"][0]["message_id"] == cursor_message_id, replay
            assert replay["next_cursor"] == cursor, replay
    product = summary["product_scope"]
    assert tool("reopen_product", "ensure_product", {"product_key": product["product"]["product_uid"]}, 400) == product["product"]
    inbox = tool("reopen_product_inbox", "fetch_inbox_product", {
        "product_key": product["product"]["product_uid"], "agent_name": "BluePeak",
        "include_bodies": True, "limit": 100}, 401)
    assert sorted(row["id"] for row in inbox if row.get("thread_id") == "kp1in-product-scope") == sorted(product["messages"][:2]), inbox
    slot = summary["advisory_slots"]["slot"]
    renewed = tool("reopen_slot_renew", "renew_build_slot", dict(slot, agent_name="RedFox", extend_seconds=60), 402)
    assert renewed["renewed"] is True and renewed["expires_ts"] > summary["advisory_slots"]["renewed_expires_ts"], renewed
    released = tool("reopen_slot_release", "release_build_slot", dict(slot, agent_name="RedFox"), 403)
    assert released["released"] is True and released["released_at"], released
    no_renew = tool("reopen_slot_released_renew", "renew_build_slot", dict(slot, agent_name="RedFox"), 404)
    assert no_renew["renewed"] is False, no_renew
    shared = tool("reopen_slot_shared", "acquire_build_slot", dict(slot, agent_name="GreenLake",
        ttl_seconds=120, exclusive=False), 405)
    assert shared["conflicts"] == [] and shared["granted"]["agent"] == "GreenLake", shared
    for index, actor in enumerate(["BluePeak", "GreenLake"]):
        assert tool("reopen_slot_release_" + actor, "release_build_slot", dict(slot, agent_name=actor), 406 + index)["released"] is True
    leases = [json.loads(path.read_text()) for path in Path(storage).glob(
        "projects/*/build_slots/kp1in-advisory-slot/*.json")]
    assert sorted(row["agent"] for row in leases) == ["BluePeak", "GreenLake", "RedFox"], leases
    assert all(row.get("released_ts") for row in leases), leases
    stop(signal.SIGINT)
    summary["passed"] = True
except BaseException as error:
    summary["error"] = repr(error)
finally:
    if server is not None:
        if server.poll() is None:
            os.killpg(server.pid, signal.SIGTERM)
            try:
                server.wait(timeout=75)
            except subprocess.TimeoutExpired:
                os.killpg(server.pid, signal.SIGKILL)
                server.wait(timeout=5)
                summary["forced_cleanup"] = True
        summary["servers"][-1]["exit"] = server.returncode
    summary["elapsed_s"] = time.monotonic() - started
    (run / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
print(json.dumps(summary))
raise SystemExit(0 if summary["passed"] else 1)
PY
then
    e2e_pass "HTTP send/read/ack survives clean SIGTERM drain and SIGINT reopen shutdown"
else
    e2e_fail "HTTP handoff, clean signal shutdown or persisted reopen failed"
fi

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
