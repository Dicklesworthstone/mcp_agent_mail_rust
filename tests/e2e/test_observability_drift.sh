#!/usr/bin/env bash
set -u

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
SCRIPT="$ROOT/scripts/check_observability_drift.py"
LOG_DIR="${LOG_DIR:-$ROOT/tests/artifacts/observability_drift/$(date -u +%Y%m%dT%H%M%SZ)}"

if [[ "${1:-}" == "--replay" && -n "${2:-}" ]]; then
  LOG_DIR="$2"
fi

mkdir -p "$LOG_DIR"

passed=0
failed=0

make_schema() {
  local fixture_root="$1"
  local events_json="$2"
  local required_json="$3"
  mkdir -p "$fixture_root/docs/schemas/git_251" "$fixture_root/src"
  python3 - "$fixture_root/docs/schemas/git_251/unit.schema.json" "$events_json" "$required_json" <<'PY'
import json
import sys
from pathlib import Path

path = Path(sys.argv[1])
events = json.loads(sys.argv[2])
required = json.loads(sys.argv[3])
fields = sorted(set(required + ["repo_slug", "caller", "duration_ms"]))
schema = {
    "$schema": "https://json-schema.org/draft/2020-12/schema",
    "$id": "https://schemas.mcp-agent-mail/git_251/unit.schema.json",
    "title": "unit event",
    "description": "E2E fixture schema.",
    "type": "object",
    "additionalProperties": False,
    "required": ["ts", "level", "target", "name", "fields"],
    "x-event-names": events,
    "properties": {
        "ts": {"type": "string"},
        "level": {"type": "string"},
        "target": {"type": "string"},
        "name": {"type": "string"},
        "fields": {
            "type": "object",
            "additionalProperties": False,
            "required": required,
            "properties": {field: {"type": "string"} for field in fields},
        },
    },
}
path.write_text(json.dumps(schema), encoding="utf-8")
PY
}

assert_category() {
  local output_file="$1"
  local expected_category="$2"
  if [[ -z "$expected_category" ]]; then
    python3 - "$output_file" <<'PY'
import json
import sys

payload = json.loads(open(sys.argv[1], encoding="utf-8").read())
raise SystemExit(0 if not payload["findings"] else 1)
PY
  else
    python3 - "$output_file" "$expected_category" <<'PY'
import json
import sys

payload = json.loads(open(sys.argv[1], encoding="utf-8").read())
category = sys.argv[2]
raise SystemExit(0 if any(item["category"] == category for item in payload["findings"]) else 1)
PY
  fi
}

run_scenario() {
  local name="$1"
  local expected_exit="$2"
  local expected_category="$3"
  shift 3

  local dir="$LOG_DIR/$name"
  mkdir -p "$dir"

  "$@" >"$dir/stdout.log" 2>"$dir/stderr.log"
  local status=$?
  printf '%s\n' "$status" >"$dir/exit"
  cp "$dir/stdout.log" "$dir/findings.json"

  if [[ "$status" -eq "$expected_exit" ]] && assert_category "$dir/stdout.log" "$expected_category"; then
    passed=$((passed + 1))
    printf '{"scenario":"%s","status":"passed","exit":%s}\n' "$name" "$status"
  else
    failed=$((failed + 1))
    printf '{"scenario":"%s","status":"failed","exit":%s,"expected_exit":%s,"log_dir":"%s"}\n' \
      "$name" "$status" "$expected_exit" "$dir"
  fi
}

clean="$LOG_DIR/fixtures/clean"
make_schema "$clean" '["git_locked_exit_ok"]' '["caller"]'
cat >"$clean/src/lib.rs" <<'RS'
pub fn emit() {
    tracing::info!(
        target: "mcp_agent_mail::git_locked",
        caller = "unit_test",
        "git_locked_exit_ok"
    );
}
RS

code_ahead="$LOG_DIR/fixtures/code_ahead"
make_schema "$code_ahead" '["git_locked_exit_ok"]' '[]'
cat >"$code_ahead/src/lib.rs" <<'RS'
pub fn emit() {
    tracing::warn!(
        target: "mcp_agent_mail::git_locked",
        caller = "unit_test",
        "git_new_event"
    );
}
RS

doc_ahead="$LOG_DIR/fixtures/doc_ahead"
make_schema "$doc_ahead" '["git_old_event"]' '[]'
cat >"$doc_ahead/src/lib.rs" <<'RS'
pub fn emit() {}
RS

field_mismatch="$LOG_DIR/fixtures/field_mismatch"
make_schema "$field_mismatch" '["git_locked_exit_ok"]' '["repo_slug", "caller"]'
cat >"$field_mismatch/src/lib.rs" <<'RS'
pub fn emit() {
    tracing::info!(
        target: "mcp_agent_mail::git_locked",
        repo_slug = "repo",
        "git_locked_exit_ok"
    );
}
RS

run_scenario \
  s1_clean_fixture_no_drift \
  0 \
  "" \
  python3 "$SCRIPT" --scan-root "$clean" --schemas-dir "$clean/docs/schemas/git_251" \
    --spec-doc "$clean/missing.md" --target-prefix "mcp_agent_mail::git_locked"

run_scenario \
  s2_simulate_code_drift \
  1 \
  code_ahead \
  python3 "$SCRIPT" --scan-root "$code_ahead" --schemas-dir "$code_ahead/docs/schemas/git_251" \
    --spec-doc "$code_ahead/missing.md" --target-prefix "mcp_agent_mail::git_locked"

run_scenario \
  s3_simulate_doc_drift \
  1 \
  doc_ahead \
  python3 "$SCRIPT" --scan-root "$doc_ahead" --schemas-dir "$doc_ahead/docs/schemas/git_251" \
    --spec-doc "$doc_ahead/missing.md" --target-prefix "mcp_agent_mail::git_locked"

run_scenario \
  s4_simulate_field_mismatch \
  1 \
  field_mismatch \
  python3 "$SCRIPT" --scan-root "$field_mismatch" --schemas-dir "$field_mismatch/docs/schemas/git_251" \
    --spec-doc "$field_mismatch/missing.md" --target-prefix "mcp_agent_mail::git_locked"

run_scenario \
  s5_real_repo_consistency \
  0 \
  "" \
  python3 "$SCRIPT" --scan-root "$ROOT/crates" --schemas-dir "$ROOT/docs/schemas/git_251" \
    --spec-doc "$ROOT/docs/OBSERVABILITY_git_251.md"

summary="$LOG_DIR/summary.json"
python3 - "$passed" "$failed" "$LOG_DIR" >"$summary" <<'PY'
import json
import sys
from datetime import datetime, timezone

passed = int(sys.argv[1])
failed = int(sys.argv[2])
log_dir = sys.argv[3]
print(json.dumps({
    "ts": datetime.now(timezone.utc).isoformat(),
    "test": "observability_drift",
    "summary": {
        "scenarios": passed + failed,
        "passed": passed,
        "failed": failed,
        "log_dir": log_dir,
        "replay_command": f"bash tests/e2e/test_observability_drift.sh --replay {log_dir}",
    },
}, sort_keys=True))
PY
cat "$summary"

if [[ "$failed" -ne 0 ]]; then
  exit 1
fi
