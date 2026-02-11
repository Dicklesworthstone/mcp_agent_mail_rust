#!/usr/bin/env bash
# e2e_dual_mode.sh - Dual-mode E2E suite with rich structured logging (br-21gj.5.6)
#
# Tests MCP-default-deny + CLI-opt-in-allow across all parity command families
# with per-step JSON logs, failure bundles, and deterministic output formatting.
#
# Usage:
#   bash scripts/e2e_dual_mode.sh
#
# Artifacts:
#   tests/artifacts/dual_mode/<timestamp>/
#   - steps/*.json           (per-step structured logs)
#   - summary.json           (aggregate summary)
#   - failures/*.json        (failure bundles with reproduction commands)

E2E_SUITE="dual_mode"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-/data/tmp/cargo-target}"
CLI_BIN="${CARGO_TARGET_DIR}/debug/am"
MCP_BIN="${CARGO_TARGET_DIR}/debug/mcp-agent-mail"

# ---------------------------------------------------------------------------
# Build binaries if needed
# ---------------------------------------------------------------------------

if [ ! -f "$CLI_BIN" ]; then
    e2e_banner "Building CLI binary..."
    cargo build -p mcp-agent-mail-cli 2>&1 | tail -3
fi
if [ ! -f "$MCP_BIN" ]; then
    e2e_banner "Building MCP binary..."
    cargo build -p mcp-agent-mail 2>&1 | tail -3
fi
if [ ! -f "$CLI_BIN" ] || [ ! -f "$MCP_BIN" ]; then
    echo "FATAL: Required binaries not found" >&2
    exit 1
fi

# ---------------------------------------------------------------------------
# Environment setup
# ---------------------------------------------------------------------------

e2e_init_artifacts
e2e_banner "Dual-Mode E2E Suite (br-21gj.5.6)"

TMPD=$(e2e_mktemp "dual_mode")
export DATABASE_URL="sqlite:///${TMPD}/test.sqlite3"
export STORAGE_ROOT="${TMPD}/storage"
export AGENT_NAME="DualModeTest"
export HTTP_HOST="127.0.0.1"
export HTTP_PORT="1"
export HTTP_PATH="/mcp/"
mkdir -p "$STORAGE_ROOT"

STEPS_DIR="${E2E_ARTIFACT_DIR}/steps"
FAILURES_DIR="${E2E_ARTIFACT_DIR}/failures"
mkdir -p "$STEPS_DIR" "$FAILURES_DIR"

STEP_NUM=0

# ---------------------------------------------------------------------------
# Structured step logger (pure bash, no python3)
# ---------------------------------------------------------------------------

# Escape a string for safe JSON embedding (handle quotes, backslashes, newlines).
json_escape() {
    local s="$1"
    s="${s//\\/\\\\}"
    s="${s//\"/\\\"}"
    s="${s//$'\n'/\\n}"
    s="${s//$'\r'/}"
    s="${s//$'\t'/\\t}"
    # Truncate to 500 chars
    [ ${#s} -gt 500 ] && s="${s:0:500}..."
    printf '%s' "$s"
}

log_step() {
    local binary="$1" command="$2" mode="$3" expected_decision="$4"
    local actual_exit="$5" stdout_excerpt="$6" stderr_excerpt="$7" passed="$8"
    local step_ts
    step_ts=$(date -u +%Y-%m-%dT%H:%M:%SZ)

    STEP_NUM=$((STEP_NUM + 1))
    local step_id
    step_id=$(printf "%03d" "$STEP_NUM")

    local safe_cmd safe_stdout safe_stderr
    safe_cmd=$(json_escape "$command")
    safe_stdout=$(json_escape "$stdout_excerpt")
    safe_stderr=$(json_escape "$stderr_excerpt")

    cat > "${STEPS_DIR}/step_${step_id}.json" <<EOJSON
{
  "step_id": "${step_id}",
  "timestamp": "${step_ts}",
  "binary": "${binary}",
  "command": "${safe_cmd}",
  "mode": "${mode}",
  "mode_provenance": "binary-level (compile-time separation)",
  "expected_decision": "${expected_decision}",
  "actual_exit_code": ${actual_exit},
  "stdout_excerpt": "${safe_stdout}",
  "stderr_excerpt": "${safe_stderr}",
  "passed": ${passed}
}
EOJSON

    if [ "$passed" = "false" ]; then
        cat > "${FAILURES_DIR}/fail_${step_id}.json" <<EOJSON
{
  "step_id": "${step_id}",
  "binary": "${binary}",
  "command": "${safe_cmd}",
  "mode": "${mode}",
  "expected_decision": "${expected_decision}",
  "actual_exit_code": ${actual_exit},
  "stdout": "${safe_stdout}",
  "stderr": "${safe_stderr}",
  "reproduction": "${binary} ${safe_cmd}"
}
EOJSON
    fi
}

# ═══════════════════════════════════════════════════════════════════════
# Section 1: CLI-allow (all 22 command families)
# ═══════════════════════════════════════════════════════════════════════

e2e_banner "Section 1: CLI binary accepts all command families"

CLI_ALLOW=(
    "serve-http --help"
    "serve-stdio --help"
    "share --help"
    "archive --help"
    "guard --help"
    "acks --help"
    "list-acks --help"
    "migrate --help"
    "list-projects --help"
    "clear-and-reset-everything --help"
    "config --help"
    "amctl --help"
    "projects --help"
    "mail --help"
    "products --help"
    "docs --help"
    "doctor --help"
    "agents --help"
    "tooling --help"
    "macros --help"
    "contacts --help"
    "file_reservations --help"
)

for entry in "${CLI_ALLOW[@]}"; do
    read -ra args <<< "$entry"
    cmd_name="${args[0]}"

    exit_code=0
    "$CLI_BIN" "${args[@]}" > /dev/null 2>&1 || exit_code=$?

    if [ "$exit_code" -eq 0 ]; then
        passed="true"
        e2e_pass "CLI allows $cmd_name"
    else
        passed="false"
        e2e_fail "CLI rejects $cmd_name (exit $exit_code)"
    fi

    log_step "am" "$entry" "cli" "allow" "$exit_code" "" "" "$passed"
done

# ═══════════════════════════════════════════════════════════════════════
# Section 2: MCP-deny (16 CLI-only commands)
# ═══════════════════════════════════════════════════════════════════════

e2e_banner "Section 2: MCP binary denies CLI-only commands"

MCP_DENY=(
    share archive guard acks migrate list-projects
    clear-and-reset-everything doctor agents tooling
    macros contacts mail projects products file_reservations
)

for cmd in "${MCP_DENY[@]}"; do
    exit_code=0
    stderr_out=$("$MCP_BIN" "$cmd" 2>&1 >/dev/null) || exit_code=$?

    if [ "$exit_code" -eq 2 ]; then
        passed="true"
        e2e_pass "MCP denies $cmd (exit 2)"
    else
        passed="false"
        e2e_fail "MCP allows $cmd (exit $exit_code, expected 2)"
    fi

    log_step "mcp-agent-mail" "$cmd" "mcp" "deny" "$exit_code" "" "$stderr_out" "$passed"
done

# ═══════════════════════════════════════════════════════════════════════
# Section 3: MCP-allow (server commands)
# ═══════════════════════════════════════════════════════════════════════

e2e_banner "Section 3: MCP binary allows server commands"

MCP_ALLOW=(
    "serve --help"
    "config"
    "--help"
    "--version"
)

for entry in "${MCP_ALLOW[@]}"; do
    read -ra args <<< "$entry"

    exit_code=0
    "$MCP_BIN" "${args[@]}" > /dev/null 2>&1 || exit_code=$?

    if [ "$exit_code" -eq 0 ]; then
        passed="true"
        e2e_pass "MCP allows $entry"
    else
        passed="false"
        e2e_fail "MCP rejects $entry (exit $exit_code)"
    fi

    log_step "mcp-agent-mail" "$entry" "mcp" "allow" "$exit_code" "" "" "$passed"
done

# ═══════════════════════════════════════════════════════════════════════
# Section 4: Denial message contract validation
# ═══════════════════════════════════════════════════════════════════════

e2e_banner "Section 4: Denial message quality"

DENIAL_TEST_CMDS=(share guard doctor archive migrate)

for cmd in "${DENIAL_TEST_CMDS[@]}"; do
    denial_stderr=$("$MCP_BIN" "$cmd" 2>&1) || true

    # Contract: must contain command name in error line
    e2e_assert_contains "denial[$cmd] mentions command" "$denial_stderr" "\"${cmd}\""
    e2e_assert_contains "denial[$cmd] has remediation" "$denial_stderr" "am ${cmd}"
    e2e_assert_contains "denial[$cmd] lists accepted" "$denial_stderr" "serve, config"
    e2e_assert_not_contains "denial[$cmd] no panic" "$denial_stderr" "panicked"
    e2e_assert_not_contains "denial[$cmd] no backtrace" "$denial_stderr" "stack backtrace"

    # Contract: stdout must be empty
    denial_stdout=$("$MCP_BIN" "$cmd" 2>/dev/null) || true
    if [ -z "$denial_stdout" ]; then
        e2e_pass "denial[$cmd] stdout is empty"
    else
        e2e_fail "denial[$cmd] stdout is not empty"
    fi
done

# ═══════════════════════════════════════════════════════════════════════
# Section 5: Security — env var cannot bypass MCP denial
# ═══════════════════════════════════════════════════════════════════════

e2e_banner "Section 5: Env override cannot bypass MCP denial"

for env_override in "INTERFACE_MODE=agent" "INTERFACE_MODE=cli" "MCP_MODE=agent"; do
    env_key="${env_override%%=*}"
    env_val="${env_override#*=}"

    exit_code=0
    env "${env_override}" "$MCP_BIN" share > /dev/null 2>&1 || exit_code=$?

    e2e_assert_eq "env ${env_override} → still denied" "2" "$exit_code"
    log_step "mcp-agent-mail" "share (${env_override})" "mcp-env-override" "deny" "$exit_code" "" "" "$([ $exit_code -eq 2 ] && echo true || echo false)"
done

# ═══════════════════════════════════════════════════════════════════════
# Section 6: Cross-mode parity — config accepted by both
# ═══════════════════════════════════════════════════════════════════════

e2e_banner "Section 6: Cross-mode parity"

cli_exit=0
"$CLI_BIN" config --help > /dev/null 2>&1 || cli_exit=$?
mcp_exit=0
"$MCP_BIN" config > /dev/null 2>&1 || mcp_exit=$?

if [ "$cli_exit" -eq 0 ] && [ "$mcp_exit" -eq 0 ]; then
    e2e_pass "config accepted by both binaries"
else
    e2e_fail "config parity: CLI=$cli_exit MCP=$mcp_exit"
fi

# ═══════════════════════════════════════════════════════════════════════
# Section 7: CLI functional operations (with actual DB)
# ═══════════════════════════════════════════════════════════════════════

e2e_banner "Section 7: CLI functional operations"

# Migrate
exit_code=0
"$CLI_BIN" migrate > /dev/null 2>&1 || exit_code=$?
e2e_assert_eq "CLI migrate exits 0" "0" "$exit_code"

# Doctor check
exit_code=0
doctor_out=$("$CLI_BIN" doctor check --json 2>/dev/null) || exit_code=$?
e2e_assert_eq "CLI doctor check exits 0" "0" "$exit_code"
e2e_assert_contains "doctor output has healthy" "$doctor_out" "healthy"

# List projects (empty state)
exit_code=0
projects_out=$("$CLI_BIN" list-projects --json 2>/dev/null) || exit_code=$?
e2e_assert_eq "CLI list-projects exits 0" "0" "$exit_code"

# Tooling directory
exit_code=0
tooling_out=$("$CLI_BIN" tooling directory --json 2>/dev/null) || exit_code=$?
e2e_assert_eq "CLI tooling directory exits 0" "0" "$exit_code"
e2e_assert_contains "tooling has clusters" "$tooling_out" "clusters"

# Tooling schemas
exit_code=0
schemas_out=$("$CLI_BIN" tooling schemas --json 2>/dev/null) || exit_code=$?
e2e_assert_eq "CLI tooling schemas exits 0" "0" "$exit_code"

# Agents list (help — requires --project arg we don't have seeded)
exit_code=0
"$CLI_BIN" agents list --help > /dev/null 2>&1 || exit_code=$?
e2e_assert_eq "CLI agents list --help exits 0" "0" "$exit_code"

# ═══════════════════════════════════════════════════════════════════════
# Summary
# ═══════════════════════════════════════════════════════════════════════

# Count step artifacts.
# Use `find` instead of `ls|wc` so `set -o pipefail` never yields a duplicate
# fallback value in command substitution when no files match.
total_steps=$(find "${STEPS_DIR}" -maxdepth 1 -type f -name 'step_*.json' | wc -l | tr -d '[:space:]')
fail_count=$(find "${FAILURES_DIR}" -maxdepth 1 -type f -name 'fail_*.json' | wc -l | tr -d '[:space:]')

e2e_save_artifact "run_summary.json" "{
  \"suite\": \"dual_mode\",
  \"total_steps\": ${total_steps},
  \"step_failures\": ${fail_count},
  \"e2e_pass\": ${_E2E_PASS},
  \"e2e_fail\": ${_E2E_FAIL},
  \"e2e_skip\": ${_E2E_SKIP}
}"

e2e_summary
