#!/usr/bin/env bash
# post_cutover_cleanup_agent_mail.sh - deterministic post-cutover cleanup verifier
#
# Required by bd-3un.37.2 acceptance criteria.
# Verifies retained frankensearch search path behavior and emits deterministic
# artifacts for replay/debug.

set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
LOG_ROOT="${ROOT_DIR}/test_logs/post_cutover_cleanup"

MODE="smoke"
EXECUTION_MODE="live"

usage() {
  cat <<'USAGE'
Usage: scripts/e2e/post_cutover_cleanup_agent_mail.sh [--mode smoke|all] [--execution live|dry] [--dry-run]

Options:
  --mode <smoke|all>      smoke: compile/lint/unit/db checks, all: smoke + Search V3 stdio e2e.
  --execution <live|dry>  live executes commands, dry records deterministic skipped-stage artifacts.
  --dry-run               Alias for --execution dry.
  -h, --help              Show this help text.

Environment:
  POST_CUTOVER_FORCE_LOCAL_CIRCUIT=1  Prefixes cargo stages with RCH_MOCK_CIRCUIT_OPEN=1.
USAGE
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --mode)
      MODE="$2"
      shift 2
      ;;
    --execution)
      EXECUTION_MODE="$2"
      shift 2
      ;;
    --dry-run)
      EXECUTION_MODE="dry"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "$MODE" != "smoke" && "$MODE" != "all" ]]; then
  echo "Invalid --mode: $MODE (expected smoke|all)" >&2
  exit 2
fi

if [[ "$EXECUTION_MODE" != "live" && "$EXECUTION_MODE" != "dry" ]]; then
  echo "Invalid --execution: $EXECUTION_MODE (expected live|dry)" >&2
  exit 2
fi

json_escape() {
  python3 -c 'import json,sys; print(json.dumps(sys.argv[1])[1:-1])' "$1"
}

now_iso() {
  date -u '+%Y-%m-%dT%H:%M:%SZ'
}

RUN_ID="post-cutover-cleanup-agent-mail-$(date -u '+%Y%m%dT%H%M%SZ')-$(date +%s%N)-$$"
RUN_DIR="${LOG_ROOT}/${RUN_ID}"
RUN_SUFFIX="${RUN_ID##*-}"
CARGO_TARGET_DIR="${POST_CUTOVER_CARGO_TARGET_DIR:-/data/tmp/target-post-cutover-cleanup-${RUN_SUFFIX}}"
EVENTS_JSONL="${RUN_DIR}/events.jsonl"
TRANSCRIPT_TXT="${RUN_DIR}/terminal_transcript.txt"
SUMMARY_JSON="${RUN_DIR}/summary.json"
SUMMARY_MD="${RUN_DIR}/summary.md"
MANIFEST_JSON="${RUN_DIR}/manifest.json"
REPLAY_TXT="${RUN_DIR}/decommission_replay_command.txt"

mkdir -p "${RUN_DIR}"
: > "${EVENTS_JSONL}"
: > "${TRANSCRIPT_TXT}"

cat > "${REPLAY_TXT}" <<EOF_REPLAY
cd ${ROOT_DIR} && POST_CUTOVER_CARGO_TARGET_DIR=${CARGO_TARGET_DIR} POST_CUTOVER_FORCE_LOCAL_CIRCUIT=${POST_CUTOVER_FORCE_LOCAL_CIRCUIT:-0} scripts/e2e/post_cutover_cleanup_agent_mail.sh --mode ${MODE} --execution ${EXECUTION_MODE}
EOF_REPLAY

STAGE_STARTED_COUNT=0
STAGE_COMPLETED_COUNT=0
ACTIVE_STAGE=""
RUN_STATUS="ok"
RUN_REASON_CODE="post_cutover_cleanup.session.ok"
RUN_MESSAGE="session initialized"
LAST_FAILURE_STAGE=""
LAST_FAILURE_EXIT_CODE=""
LAST_FAILURE_REASON_CODE=""
FINALIZED=0

if [[ "${POST_CUTOVER_FORCE_LOCAL_CIRCUIT:-0}" == "1" ]]; then
  RCH_PREFIX="RCH_MOCK_CIRCUIT_OPEN=1 rch exec --"
else
  RCH_PREFIX="rch exec --"
fi

emit_event() {
  local stage="$1"
  local status="$2"
  local reason_code="$3"
  local detail="$4"
  printf '{"run_id":"%s","ts":"%s","mode":"%s","execution_mode":"%s","stage":"%s","status":"%s","reason_code":"%s","detail":"%s"}\n' \
    "$(json_escape "${RUN_ID}")" \
    "$(now_iso)" \
    "$(json_escape "${MODE}")" \
    "$(json_escape "${EXECUTION_MODE}")" \
    "$(json_escape "${stage}")" \
    "$(json_escape "${status}")" \
    "$(json_escape "${reason_code}")" \
    "$(json_escape "${detail}")" \
    >> "${EVENTS_JSONL}"
}

record_command_header() {
  local stage="$1"
  local cmd="$2"
  {
    printf '\n[%s] stage=%s command=%s\n' "$(now_iso)" "${stage}" "${cmd}"
  } >> "${TRANSCRIPT_TXT}"
}

mark_stage_started() {
  local stage="$1"
  ACTIVE_STAGE="$stage"
  STAGE_STARTED_COUNT=$((STAGE_STARTED_COUNT + 1))
}

mark_stage_completed() {
  ACTIVE_STAGE=""
  STAGE_COMPLETED_COUNT=$((STAGE_COMPLETED_COUNT + 1))
}

set_status() {
  RUN_STATUS="$1"
  RUN_REASON_CODE="$2"
  RUN_MESSAGE="$3"
}

classify_stage_failure_reason() {
  local exit_code="$1"
  if [[ "$exit_code" -eq 124 || "$exit_code" -eq 137 || "$exit_code" -eq 143 ]]; then
    printf '%s' "post_cutover_cleanup.stage.timeout"
    return
  fi
  printf '%s' "post_cutover_cleanup.stage.failed"
}

run_stage() {
  local stage="$1"
  local cmd="$2"
  mark_stage_started "$stage"
  emit_event "$stage" "running" "post_cutover_cleanup.stage.started" "Stage started"

  if [[ "$EXECUTION_MODE" == "dry" ]]; then
    record_command_header "$stage" "$cmd"
    printf '[dry-run] skipped execution\n' >> "${TRANSCRIPT_TXT}"
    emit_event "$stage" "ok" "post_cutover_cleanup.stage.skipped_dry_run" "Dry-run mode"
    mark_stage_completed
    return 0
  fi

  record_command_header "$stage" "$cmd"
  set +e
  bash -lc "cd \"${ROOT_DIR}\" && ${cmd}" >> "${TRANSCRIPT_TXT}" 2>&1
  local exit_code=$?
  set -e

  if [[ "$exit_code" -eq 0 ]]; then
    emit_event "$stage" "ok" "post_cutover_cleanup.stage.ok" "Stage completed"
    mark_stage_completed
    return 0
  fi

  local reason
  reason="$(classify_stage_failure_reason "$exit_code")"
  LAST_FAILURE_STAGE="$stage"
  LAST_FAILURE_EXIT_CODE="$exit_code"
  LAST_FAILURE_REASON_CODE="$reason"
  emit_event "$stage" "fail" "$reason" "Command failed (exit=${exit_code})"
  mark_stage_completed
  return "$exit_code"
}

write_summary() {
  cat > "${SUMMARY_JSON}" <<EOF_SUMMARY
{"schema":"post-cutover-cleanup-summary-v1","v":1,"run_id":"$(json_escape "${RUN_ID}")","status":"$(json_escape "${RUN_STATUS}")","reason_code":"$(json_escape "${RUN_REASON_CODE}")","message":"$(json_escape "${RUN_MESSAGE}")","mode":"$(json_escape "${MODE}")","execution_mode":"$(json_escape "${EXECUTION_MODE}")","cargo_target_dir":"$(json_escape "${CARGO_TARGET_DIR}")","stage_started_count":${STAGE_STARTED_COUNT},"stage_completed_count":${STAGE_COMPLETED_COUNT},"active_stage":"$(json_escape "${ACTIVE_STAGE}")","last_failure_stage":"$(json_escape "${LAST_FAILURE_STAGE}")","last_failure_exit_code":"$(json_escape "${LAST_FAILURE_EXIT_CODE}")","events":"$(json_escape "${EVENTS_JSONL}")","transcript":"$(json_escape "${TRANSCRIPT_TXT}")","replay":"$(json_escape "${REPLAY_TXT}")","ts":"$(now_iso)"}
EOF_SUMMARY

  cat > "${SUMMARY_MD}" <<EOF_SUMMARY_MD
# Post-Cutover Cleanup Validation Summary

- run_id: \`${RUN_ID}\`
- status: **${RUN_STATUS}**
- reason_code: \`${RUN_REASON_CODE}\`
- mode: \`${MODE}\`
- execution_mode: \`${EXECUTION_MODE}\`
- cargo_target_dir: \`${CARGO_TARGET_DIR}\`
- stage_started_count: ${STAGE_STARTED_COUNT}
- stage_completed_count: ${STAGE_COMPLETED_COUNT}
- active_stage: \`${ACTIVE_STAGE:-<none>}\`
- last_failure_stage: \`${LAST_FAILURE_STAGE:-<none>}\`
- last_failure_exit_code: \`${LAST_FAILURE_EXIT_CODE:-<none>}\`

## Artifacts

- events: \`${EVENTS_JSONL}\`
- transcript: \`${TRANSCRIPT_TXT}\`
- replay: \`${REPLAY_TXT}\`
- summary_json: \`${SUMMARY_JSON}\`
- manifest: \`${MANIFEST_JSON}\`

EOF_SUMMARY_MD

  cat > "${MANIFEST_JSON}" <<EOF_MANIFEST
{"schema":"post-cutover-cleanup-manifest-v1","v":1,"run_id":"$(json_escape "${RUN_ID}")","status":"$(json_escape "${RUN_STATUS}")","reason_code":"$(json_escape "${RUN_REASON_CODE}")","mode":"$(json_escape "${MODE}")","execution_mode":"$(json_escape "${EXECUTION_MODE}")","cargo_target_dir":"$(json_escape "${CARGO_TARGET_DIR}")","stage_started_count":${STAGE_STARTED_COUNT},"stage_completed_count":${STAGE_COMPLETED_COUNT},"active_stage":"$(json_escape "${ACTIVE_STAGE}")","artifacts":{"events":"$(json_escape "${EVENTS_JSONL}")","transcript":"$(json_escape "${TRANSCRIPT_TXT}")","replay":"$(json_escape "${REPLAY_TXT}")","summary_json":"$(json_escape "${SUMMARY_JSON}")","summary_md":"$(json_escape "${SUMMARY_MD}")"},"ts":"$(now_iso)"}
EOF_MANIFEST
}

finalize() {
  local exit_code="$1"
  if [[ "$FINALIZED" -eq 1 ]]; then
    return
  fi
  FINALIZED=1

  if [[ "$exit_code" -ne 0 && "$RUN_STATUS" == "ok" ]]; then
    if [[ -n "$LAST_FAILURE_REASON_CODE" ]]; then
      set_status "fail" "$LAST_FAILURE_REASON_CODE" "Stage failed: ${LAST_FAILURE_STAGE} (exit=${LAST_FAILURE_EXIT_CODE})"
    else
      set_status "fail" "post_cutover_cleanup.session.failed" "Session failed"
    fi
  fi

  write_summary
  emit_event "session.finalize" "$RUN_STATUS" "$RUN_REASON_CODE" "$RUN_MESSAGE"

  if [[ "$RUN_STATUS" == "ok" ]]; then
    printf 'Artifacts: %s\n' "$RUN_DIR"
  else
    printf 'Artifacts (failed run): %s\n' "$RUN_DIR" >&2
  fi
}

on_signal() {
  local signal_name="$1"
  set_status "fail" "post_cutover_cleanup.session.interrupted" "Received signal ${signal_name}"
  emit_event "session.interrupted" "fail" "post_cutover_cleanup.session.interrupted" "Interrupted by ${signal_name}"
  exit 143
}

on_exit() {
  local rc=$?
  finalize "$rc"
  exit "$rc"
}

trap 'on_signal INT' INT
trap 'on_signal TERM' TERM
trap 'on_signal HUP' HUP
trap on_exit EXIT

emit_event "session.init" "ok" "post_cutover_cleanup.session.init" "Session initialized"

run_stage "compile.search_core_hybrid" "env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} ${RCH_PREFIX} cargo check -p mcp-agent-mail-search-core --all-targets --features hybrid"
run_stage "lint.search_core_hybrid" "env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} ${RCH_PREFIX} cargo clippy -p mcp-agent-mail-search-core --all-targets --features hybrid -- -D warnings"
run_stage "unit.search_core.fs_probe_doc_key_roundtrip" "env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} ${RCH_PREFIX} cargo test -p mcp-agent-mail-search-core --features hybrid fs_probe_doc_key_roundtrip -- --nocapture"
run_stage "unit.search_core.from_fs_phase" "env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} ${RCH_PREFIX} cargo test -p mcp-agent-mail-search-core --features hybrid from_fs_phase_ -- --nocapture"
run_stage "integration.db.select_best_two_tier_results" "env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} ${RCH_PREFIX} cargo test -p mcp-agent-mail-db --features hybrid select_best_two_tier_results -- --nocapture"

if [[ "$MODE" == "all" ]]; then
  run_stage "e2e.search_v3_stdio" "env CARGO_TARGET_DIR=${CARGO_TARGET_DIR} ${RCH_PREFIX} bash tests/e2e/test_search_v3_stdio.sh"
fi

if command -v ubs >/dev/null 2>&1; then
  run_stage "scan.ubs.changed_script" "ubs scripts/e2e/post_cutover_cleanup_agent_mail.sh"
fi

set_status "ok" "post_cutover_cleanup.lane.passed" "Post-cutover cleanup validation lane passed"
exit 0
