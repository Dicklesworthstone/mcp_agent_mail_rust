#!/usr/bin/env bash
set -euo pipefail

TEST_NAME="stress_known_bad_git"
MODE="${1:-a}"
LOG_DIR="tests/artifacts/stress/git_251/$(date -u +%Y%m%dT%H%M%SZ)-e2e"
mkdir -p "$LOG_DIR"

json_escape() {
  python3 -c 'import json,sys; print(json.dumps(sys.stdin.read()))'
}

log_event() {
  local level="$1" scenario="$2" message="$3" data="${4:-{}}"
  printf '{"ts":"%s","test":"%s","scenario":"%s","level":"%s","message":%s,"data":%s}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
    "$TEST_NAME" \
    "$scenario" \
    "$level" \
    "$(json_escape <<< "$message")" \
    "$data" | tee -a "$LOG_DIR/events.jsonl"
}

run_scenario() {
  local scenario="$1" filter="$2"
  local dir="$LOG_DIR/$scenario"
  mkdir -p "$dir"
  log_event info "$scenario" "scenario started" '{"filter":"'"$filter"'"}'
  set +e
  cargo test -p mcp-agent-mail-storage --test stress_pipeline_known_bad_git "$filter" -- --nocapture \
    >"$dir/stdout.log" 2>"$dir/stderr.log"
  local status=$?
  set -e
  printf '%s\n' "$status" > "$dir/exit"
  printf '#!/usr/bin/env bash\nset -euo pipefail\ncargo test -p mcp-agent-mail-storage --test stress_pipeline_known_bad_git %q -- --nocapture\n' "$filter" > "$dir/replay.sh"
  chmod +x "$dir/replay.sh"
  if [[ "$status" -eq 0 ]]; then
    log_event pass "$scenario" "scenario passed" '{"exit":0}'
  else
    log_event fail "$scenario" "scenario failed" '{"exit":'"$status"'}'
  fi
  return "$status"
}

passed=0
failed=0

case "$MODE" in
  a)
    if run_scenario scenario_a scenario_a_clean_baseline; then passed=$((passed + 1)); else failed=$((failed + 1)); fi
    ;;
  b)
    if AM_TEST_GIT_251=1 run_scenario scenario_b scenario_b_synthetic_racer; then passed=$((passed + 1)); else failed=$((failed + 1)); fi
    ;;
  c)
    if [[ -z "${AM_GIT_BINARY:-}" ]]; then
      log_event warn scenario_c "AM_GIT_BINARY unset; scenario skipped" '{}'
    elif AM_TEST_GIT_251=1 run_scenario scenario_c scenario_c_real_2510_gated; then
      passed=$((passed + 1))
    else
      failed=$((failed + 1))
    fi
    ;;
  d)
    if [[ -z "${AM_GIT_BINARY:-}" ]]; then
      log_event warn scenario_d "AM_GIT_BINARY unset; scenario skipped" '{}'
    elif AM_TEST_GIT_251=1 AM_GIT_FLOCK_DISABLED=1 run_scenario scenario_d scenario_d_real_2510_no_flock_gated; then
      passed=$((passed + 1))
    else
      failed=$((failed + 1))
    fi
    ;;
  all)
    for item in a b c d; do
      if "$0" "$item"; then
        passed=$((passed + 1))
      else
        failed=$((failed + 1))
      fi
    done
    ;;
  *)
    echo "usage: $0 [a|b|c|d|all]" >&2
    exit 2
    ;;
esac

printf '{"ts":"%s","test":"%s","summary":{"scenarios_run":%d,"passed":%d,"failed":%d,"log_dir":"%s","replay_command":"bash %s %s"}}\n' \
  "$(date -u +%Y-%m-%dT%H:%M:%SZ)" \
  "$TEST_NAME" \
  "$((passed + failed))" \
  "$passed" \
  "$failed" \
  "$LOG_DIR" \
  "$0" \
  "$MODE" | tee -a "$LOG_DIR/events.jsonl"

[[ "$failed" -eq 0 ]]
