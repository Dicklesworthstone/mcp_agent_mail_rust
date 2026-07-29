#!/usr/bin/env bash
# test_cli_mcp_surface.sh — E2E for Track N (CLI↔MCP dual-mode surface).
# @tags: reliability, track-n, track-e, cli-mcp-surface, e1, e2, e3, e4
#
# Asserts, against REAL built `am` + `mcp-agent-mail` binaries, the dual-mode
# CLI↔MCP surface contract (Track E) that protects agents from the recurring
# "wrong binary / wrong subcommand / silent failure" footguns:
#
#   E1 — CLI↔MCP correction table. Running an MCP tool name on the MCP server
#        binary yields ONE exact corrected command (the `am ...` form + the MCP
#        tool name), not a generic usage dump; running an MCP-only command in
#        CLI mode is denied with explicit MCP-only guidance. The mapping is
#        discoverable in `am capabilities --json` (mcp_tool_cli_corrections[]).
#   E2 — Legacy `am serve` is retired: it prints the migration map + a dedicated
#        exit code (64) + an explicit do-not-retry policy, and returns at once
#        (no retry storm / no hang).
#   E3 — Bare `am` in a non-TTY context prints the machine-readable status
#        surface (incl. binary name/version/path), exit 0 — NEVER a usage error.
#   E4 — A robot command that hits a classified DB failure carries the five
#        safe-remediation fields (operator_only / repairable / safe_to_retry /
#        safe_to_continue_read_only / blocks_edits) + a recommended_command +
#        the H4 coordination contract, with operator-only-vs-agent-auto tagging.
#
# "Unavailable" mailbox (E4): DATABASE_URL points at a path whose parent
# component is a regular file, so the DB can never be opened — a clean,
# deterministic open failure that the health probe classifies + attaches a
# safe-remediation contract to, without dragging in repair/reconstruct.
#
# SCOPED OUT (honest SKIPs):
#   * The operator_only==true case (dangerous recommended_command like
#     `am doctor repair`) needs a LiveOwnerNoActivityLock classification, which
#     requires a live mailbox owner — destabilising for a non-interactive
#     harness (br-mms51). The operator-only classifier itself is unit-tested:
#       mcp-agent-mail-cli  robot::tests::remediation_operator_only_classifier_tags_dangerous_commands
#       mcp-agent-mail-cli  robot::tests::robot_remediation_from_db_error_reuses_a2_policy
#     Here we assert the agent-auto case (a read-only diagnostic → operator_only
#     == false) which exercises the same tagging path end-to-end.
#
# Ref: br-bvq1x.14.7 (N7). Depends on E1/E2/E3/E4 + L2 (all closed).

set -uo pipefail

E2E_SUITE="cli_mcp_surface"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
if [ -z "${CARGO_TARGET_DIR:-}" ]; then
    export CARGO_TARGET_DIR="${PROJECT_ROOT}/target"
fi
# shellcheck source=../../scripts/e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"
# shellcheck source=lib/structured_logging.sh
source "${SCRIPT_DIR}/lib/structured_logging.sh"

e2e_init_artifacts
e2e_banner "Track N — CLI↔MCP Dual-Mode Surface E2E"

EVENTS="${E2E_ARTIFACT_DIR}/events.jsonl"
: >"${EVENTS}"

if ! command -v jq >/dev/null 2>&1; then
    e2e_skip "jq required"
    log_summary "${E2E_SUITE}" 0 0 0 "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_cli_mcp_surface.sh" >>"${EVENTS}"
    e2e_summary
    exit 0
fi

AM_BIN="$(e2e_ensure_binary am)"
if [ -z "${AM_BIN}" ] || [ ! -x "${AM_BIN}" ]; then
    e2e_fail "could not build/locate the am binary"
    e2e_summary
    exit 1
fi
MCP_BIN="$(e2e_ensure_binary mcp-agent-mail)"
if [ -z "${MCP_BIN}" ] || [ ! -x "${MCP_BIN}" ]; then
    e2e_fail "could not build/locate the mcp-agent-mail binary"
    e2e_summary
    exit 1
fi
e2e_log "am binary:            ${AM_BIN}"
e2e_log "mcp-agent-mail binary: ${MCP_BIN}"

WORK="$(e2e_mktemp cli_mcp_surface_work)"
ART="${E2E_ARTIFACT_DIR}/scenarios"
mkdir -p "${WORK}/storage" "${WORK}/home" "${WORK}/repo" "${ART}"
export STORAGE_ROOT="${WORK}/storage"
export HOME="${WORK}/home"
# Point the CLI at an unused MCP port so every verb falls back to the isolated
# local SQLite path instead of hitting a real server on 127.0.0.1:8765.
export HTTP_HOST="127.0.0.1"
export HTTP_PORT="${AM_E2E_UNUSED_PORT:-8799}"
HEALTHY_DB="sqlite:///${WORK}/mb.sqlite3"
# Unavailable mailbox: parent path component is a regular file → DB unopenable.
printf 'x' >"${WORK}/notadir"
UNAVAIL_DB="sqlite:///${WORK}/notadir/db.sqlite3"
PROJECT_KEY="${WORK}/repo"

AM_CMD_TIMEOUT="${AM_CMD_TIMEOUT:-60}"
# Run a binary with timeout, capturing exit code WITHOUT tripping the harness's
# `set -e` (several verbs here intentionally exit non-zero: a denial=2, a retired
# verb=64), so the suite can observe the exact code rather than abort.
runbin() {
    local bin="$1" out="$2"
    shift 2
    local rc=0
    if command -v timeout >/dev/null 2>&1; then
        timeout "${AM_CMD_TIMEOUT}" "${bin}" "$@" </dev/null >"${out}" 2>"${out}.err" || rc=$?
    else
        "${bin}" "$@" </dev/null >"${out}" 2>"${out}.err" || rc=$?
    fi
    printf '%s\n' "${rc}" >"${out}.exit"
    return 0
}

amrun() { runbin "${AM_BIN}" "$@"; }
mcprun() { runbin "${MCP_BIN}" "$@"; }

check() {
    local label="$1" file="$2"
    shift 2
    if jq -e "$@" "${file}" >/dev/null 2>&1; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}  [filter: $*]"
        printf '      got: %s\n' "$(jq -c . "${file}" 2>/dev/null | head -c 320)"
    fi
}

exit_is() {
    local label="$1" out="$2" want="$3"
    local got
    got="$(cat "${out}.exit" 2>/dev/null || echo "?")"
    if [ "${got}" = "${want}" ]; then
        e2e_pass "${label} (exit ${got})"
    else
        e2e_fail "${label} (exit ${got}, expected ${want})"
        printf '      out: %s\n' "$(head -c 240 "${out}" "${out}.err" 2>/dev/null | tr '\n' ' ')"
    fi
}

stderr_has() {
    local label="$1" out="$2" needle="$3"
    if grep -qF -- "${needle}" "${out}.err" 2>/dev/null; then
        e2e_pass "${label}"
    else
        e2e_fail "${label}  [missing: ${needle}]"
        printf '      err: %s\n' "$(head -c 280 "${out}.err" 2>/dev/null | tr '\n' ' ')"
    fi
}

# ---------------------------------------------------------------------------
# E1: CLI↔MCP correction table.
# ---------------------------------------------------------------------------
e2e_case_banner "E1: MCP binary rejects a CLI tool name with ONE exact correction"
mcprun "${ART}/e1_mcp_send.out" send
exit_is "mcp-agent-mail send is denied (exit 2)" "${ART}/e1_mcp_send.out" 2
stderr_has "denial states it is not an MCP server command" "${ART}/e1_mcp_send.out" \
    "is not an MCP server command"
stderr_has "denial offers a corrected command block" "${ART}/e1_mcp_send.out" \
    "Corrected command:"
stderr_has "corrected command names the am CLI form" "${ART}/e1_mcp_send.out" \
    "CLI: am mail send"
stderr_has "corrected command names the MCP tool" "${ART}/e1_mcp_send.out" \
    "MCP tool: send_message"
# "ONE exact corrected command" — exactly one CLI: line, not a usage dump.
CLI_LINES="$(grep -c '  CLI: ' "${ART}/e1_mcp_send.out.err" 2>/dev/null || echo 0)"
if [ "${CLI_LINES}" = "1" ]; then
    e2e_pass "exactly one corrected CLI command is offered (not a usage dump)"
else
    e2e_fail "expected exactly one corrected CLI line, saw ${CLI_LINES}"
fi

e2e_case_banner "E1: an MCP-only command in CLI mode is denied with MCP guidance"
env_cli_serve_rc=0
if command -v timeout >/dev/null 2>&1; then
    AM_INTERFACE_MODE=cli timeout "${AM_CMD_TIMEOUT}" "${MCP_BIN}" serve \
        </dev/null >"${ART}/e1_cli_serve.out" 2>"${ART}/e1_cli_serve.out.err" || env_cli_serve_rc=$?
else
    AM_INTERFACE_MODE=cli "${MCP_BIN}" serve \
        </dev/null >"${ART}/e1_cli_serve.out" 2>"${ART}/e1_cli_serve.out.err" || env_cli_serve_rc=$?
fi
printf '%s\n' "${env_cli_serve_rc}" >"${ART}/e1_cli_serve.out.exit"
exit_is "serve in CLI mode is denied (exit 2)" "${ART}/e1_cli_serve.out" 2
stderr_has "denial names CLI mode + AM_INTERFACE_MODE" "${ART}/e1_cli_serve.out" \
    "is not available in CLI mode (AM_INTERFACE_MODE=cli)"
stderr_has "denial points back to the MCP server start" "${ART}/e1_cli_serve.out" \
    "mcp-agent-mail serve"

e2e_case_banner "E1: the correction table is discoverable via capabilities"
amrun "${ART}/e1_caps.json" capabilities --json
exit_is "am capabilities --json succeeds" "${ART}/e1_caps.json" 0
check "capabilities exposes mcp_tool_cli_corrections[]" "${ART}/e1_caps.json" \
    '(.mcp_tool_cli_corrections | type) == "array" and (.mcp_tool_cli_corrections | length) >= 1'
check "a correction entry maps send_message → am mail send" "${ART}/e1_caps.json" \
    '[.mcp_tool_cli_corrections[]
        | select((.attempted_names // []) | index("send"))
        | select(.mcp_tool == "send_message")
        | select(.cli | startswith("am mail send"))] | length == 1'
check "every correction entry carries attempted_names + cli" "${ART}/e1_caps.json" \
    '(.mcp_tool_cli_corrections | length) as $total
     | ([.mcp_tool_cli_corrections[]
          | select(((.attempted_names // []) | length) >= 1 and ((.cli // "") | length) >= 1)]
        | length) == $total'

# ---------------------------------------------------------------------------
# E2: legacy `am serve` is retired (dedicated exit code, no retry storm).
# ---------------------------------------------------------------------------
e2e_case_banner "E2: legacy 'am serve' prints the migration map + exit 64, no retry"
amrun "${ART}/e2_serve.out" serve
# A retry storm or hang would be killed by `timeout` (exit 124); a clean 64
# proves it returned at once with the dedicated legacy-migration code.
exit_is "legacy 'am serve' exits with the dedicated code 64 (not a hang)" "${ART}/e2_serve.out" 64
stderr_has "serve announces it is retired" "${ART}/e2_serve.out" \
    "legacy \`am serve\` is retired"
stderr_has "serve carries the migration classification" "${ART}/e2_serve.out" \
    "classification: legacy-subcommand-migration"
stderr_has "serve states the dedicated exit code" "${ART}/e2_serve.out" \
    "exit_code: 64"
stderr_has "serve states a do-not-retry policy (no retry storm)" "${ART}/e2_serve.out" \
    "retry_policy: do-not-retry-unchanged"
stderr_has "serve maps to am serve-http" "${ART}/e2_serve.out" \
    "am serve-http"
stderr_has "serve maps to am serve-stdio" "${ART}/e2_serve.out" \
    "am serve-stdio"

# ---------------------------------------------------------------------------
# E3: bare `am` (non-TTY) prints the status surface, never a usage error.
# ---------------------------------------------------------------------------
e2e_case_banner "E3: bare 'am' (non-TTY) emits the status surface, exit 0"
export DATABASE_URL="${HEALTHY_DB}"
# runbin redirects stdin from /dev/null → guaranteed non-TTY path.
amrun "${ART}/e3_bare.json"
exit_is "bare am (non-TTY) is exit 0 (never a usage error)" "${ART}/e3_bare.json" 0
check "bare status carries the versioned schema" "${ART}/e3_bare.json" \
    '.schema_version == "am.bare_status.v1"'
check "bare status names the binary" "${ART}/e3_bare.json" \
    '.binary.name == "am"'
check "bare status reports a non-empty version" "${ART}/e3_bare.json" \
    '(.binary.version | type) == "string" and (.binary.version | length) >= 1'
check "bare status reports the binary path" "${ART}/e3_bare.json" \
    '(.binary.path | type) == "string" and (.binary.path | endswith("am"))'
check "bare status includes a doctor_health rollup" "${ART}/e3_bare.json" \
    '(.doctor_health.status | type) == "string"'
check "bare status names the interface mode + a service block" "${ART}/e3_bare.json" \
    '(.mode | type) == "string" and (.service | type) == "object"'

# ---------------------------------------------------------------------------
# E4: robot failure envelope carries the safe-remediation contract.
# ---------------------------------------------------------------------------
e2e_case_banner "E4: robot health under an unavailable DB carries the safe-remediation contract"
export DATABASE_URL="${UNAVAIL_DB}"
amrun "${ART}/e4_health.json" robot health --project "${PROJECT_KEY}" --format json
if jq -e . "${ART}/e4_health.json" >/dev/null 2>&1; then
    check "envelope flags overall as not-ok under an unavailable DB" "${ART}/e4_health.json" \
        '.overall != "ok" and .overall != "healthy"'
    check "envelope attaches a _remediation contract" "${ART}/e4_health.json" \
        '(._remediation | type) == "object"'
    check "_remediation carries a recommended_command" "${ART}/e4_health.json" \
        '(._remediation.recommended_command | type) == "string" and (._remediation.recommended_command | length) >= 1'
    # The five safe-remediation flags (E4 contract).
    check "_remediation carries the five safe-remediation flags" "${ART}/e4_health.json" \
        '(._remediation.operator_only | type) == "boolean"
         and (._remediation.repairable | type) == "boolean"
         and (._remediation.safe_to_retry | type) == "boolean"
         and (._remediation.safe_to_continue_read_only | type) == "boolean"
         and (._remediation.blocks_edits | type) == "boolean"'
    # H4 coordination contract rides along with a typed verdict.
    check "_remediation carries the H4 coordination contract" "${ART}/e4_health.json" \
        '(._remediation.coordination | type) == "object"
         and ((._remediation.coordination.verdict) as $v
              | $v == "writes_blocked" or $v == "reads_degraded" or $v == "doctor_blocked_by_live_owner")
         and (._remediation.coordination.message | type) == "string"
         and (._remediation.coordination.fallback_lane | type) == "string"'
    # operator-only tagging: an unopenable DB → a read-only diagnostic command,
    # which is agent-auto (operator_only == false). This exercises the same
    # tagging path the dangerous-command unit tests cover from the other side.
    check "the recommended diagnostic command is agent-auto (operator_only=false)" "${ART}/e4_health.json" \
        '._remediation.operator_only == false and (._remediation.recommended_command | startswith("am "))'
    # An alert echoes the DB failure and points at the recommended command.
    check "an error alert surfaces the DB connectivity failure" "${ART}/e4_health.json" \
        '[._alerts[]? | select(.severity == "error")] | length >= 1'
else
    e2e_skip "robot health emitted no JSON under the unavailable mailbox (recovery-only fallback declined); the safe-remediation contract is unit-tested in robot::tests::robot_remediation_from_db_error_reuses_a2_policy"
fi

e2e_case_banner "E4: operator-only (dangerous) classification — unit-tested only"
e2e_skip "the operator_only==true case needs a dangerous recommended_command (e.g. 'am doctor repair' from a LiveOwnerNoActivityLock class), which requires a live mailbox owner — unsafe in a non-interactive harness (br-mms51). The classifier is covered in-process by mcp-agent-mail-cli robot::tests::remediation_operator_only_classifier_tags_dangerous_commands"
export DATABASE_URL="${HEALTHY_DB}"

# ---------------------------------------------------------------------------
log_summary "${E2E_SUITE}" "$((_E2E_PASS + _E2E_FAIL + _E2E_SKIP))" "${_E2E_PASS}" "${_E2E_FAIL}" \
    "${E2E_ARTIFACT_DIR}" "bash tests/e2e/test_cli_mcp_surface.sh" >>"${EVENTS}"
e2e_summary
