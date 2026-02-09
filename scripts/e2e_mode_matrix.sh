#!/usr/bin/env bash
# e2e_mode_matrix.sh - MCP-deny vs CLI-allow mode matrix E2E suite (br-21gj.5.2)
#
# Tests dual-mode routing: MCP binary denies CLI-only commands,
# CLI binary accepts all command families.

E2E_SUITE="mode_matrix"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

# ---------------------------------------------------------------------------
# Binary paths
# ---------------------------------------------------------------------------

CLI_BIN="${CARGO_TARGET_DIR:-/tmp/target-$(whoami)-am}/debug/am"
MCP_BIN="${CARGO_TARGET_DIR:-/tmp/target-$(whoami)-am}/debug/mcp-agent-mail"

if [ ! -f "$CLI_BIN" ]; then
    e2e_banner "Building CLI binary..."
    cargo build -p mcp-agent-mail-cli 2>&1 || e2e_fail "CLI binary build failed"
fi

if [ ! -f "$MCP_BIN" ]; then
    e2e_banner "Building MCP binary..."
    cargo build -p mcp-agent-mail 2>&1 || e2e_fail "MCP binary build failed"
fi

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

e2e_init_artifacts

TMPD=$(e2e_mktemp "mode_matrix")
export DATABASE_URL="sqlite:///${TMPD}/test.sqlite3"
export STORAGE_ROOT="${TMPD}/storage"
export AGENT_NAME="MatrixTestAgent"
export HTTP_HOST="127.0.0.1"
export HTTP_PORT="1"
export HTTP_PATH="/mcp/"

mkdir -p "$STORAGE_ROOT"

# ---------------------------------------------------------------------------
# Matrix: CLI binary allows all command families
# ---------------------------------------------------------------------------

e2e_banner "CLI binary: accept all command families"

CLI_ALLOW_COMMANDS=(
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

for entry in "${CLI_ALLOW_COMMANDS[@]}"; do
    read -ra args <<< "$entry"
    cmd_name="${args[0]}"
    output=$("$CLI_BIN" "${args[@]}" 2>&1)
    exit_code=$?

    log_file="${E2E_ARTIFACT_DIR}/cli_allow_${cmd_name//-/_}.log"
    {
        echo "binary: am"
        echo "command: $entry"
        echo "exit_code: $exit_code"
        echo "expected: allow (exit 0)"
        echo "---"
        echo "$output"
    } > "$log_file"

    e2e_assert_eq "CLI allows $cmd_name" "0" "$exit_code"
done

# ---------------------------------------------------------------------------
# Matrix: MCP binary denies CLI-only commands
# ---------------------------------------------------------------------------

e2e_banner "MCP binary: deny CLI-only commands"

MCP_DENY_COMMANDS=(
    "share"
    "archive"
    "guard"
    "acks"
    "migrate"
    "list-projects"
    "clear-and-reset-everything"
    "doctor"
    "agents"
    "tooling"
    "macros"
    "contacts"
    "mail"
    "projects"
    "products"
    "file_reservations"
)

for cmd in "${MCP_DENY_COMMANDS[@]}"; do
    exit_code=0
    "$MCP_BIN" "$cmd" > /dev/null 2>&1 || exit_code=$?

    log_file="${E2E_ARTIFACT_DIR}/mcp_deny_${cmd//-/_}.log"
    {
        echo "binary: mcp-agent-mail"
        echo "command: $cmd"
        echo "exit_code: $exit_code"
        echo "expected: deny (exit 2)"
    } > "$log_file"

    e2e_assert_eq "MCP denies $cmd" "2" "$exit_code"
done

# ---------------------------------------------------------------------------
# Matrix: MCP binary allows server commands
# ---------------------------------------------------------------------------

e2e_banner "MCP binary: allow server commands"

# "serve --help" should exit 0
serve_exit=0
"$MCP_BIN" serve --help > /dev/null 2>&1 || serve_exit=$?
e2e_assert_eq "MCP allows serve --help" "0" "$serve_exit"

# "config" should exit 0
config_exit=0
"$MCP_BIN" config > /dev/null 2>&1 || config_exit=$?
e2e_assert_eq "MCP allows config" "0" "$config_exit"

# ---------------------------------------------------------------------------
# MCP denial message quality (SPEC-interface-mode-switch.md)
# ---------------------------------------------------------------------------

e2e_banner "MCP denial message: remediation hints"

denial_output=$("$MCP_BIN" share 2>&1) || true

# Must mention `am` as the CLI binary remediation
hint_am=0
echo "$denial_output" | grep -q "am share" || hint_am=1
e2e_assert_eq "MCP denial mentions 'am' CLI binary" "0" "$hint_am"

# Must mention AM_INTERFACE_MODE=cli as the second remediation path
hint_mode=0
echo "$denial_output" | grep -q "AM_INTERFACE_MODE=cli" || hint_mode=1
e2e_assert_eq "MCP denial mentions AM_INTERFACE_MODE=cli" "0" "$hint_mode"

cmd_ok=0
echo "$denial_output" | grep -q "share" || cmd_ok=1
e2e_assert_eq "MCP denial mentions command name" "0" "$cmd_ok"

# ---------------------------------------------------------------------------
# AM_INTERFACE_MODE=cli: CLI mode via env var (br-163x)
# ---------------------------------------------------------------------------

e2e_banner "AM_INTERFACE_MODE=cli: CLI mode via MCP binary"

# CLI mode: --help should render CLI surface with exit 0
cli_help_exit=0
cli_help_output=$(AM_INTERFACE_MODE=cli "$MCP_BIN" --help 2>&1) || cli_help_exit=$?
e2e_assert_eq "CLI mode --help exits 0" "0" "$cli_help_exit"

cli_help_name=0
echo "$cli_help_output" | grep -q "mcp-agent-mail" || cli_help_name=1
e2e_assert_eq "CLI mode --help shows mcp-agent-mail name" "0" "$cli_help_name"

# CLI mode: share --help should be allowed
cli_share_exit=0
AM_INTERFACE_MODE=cli "$MCP_BIN" share --help > /dev/null 2>&1 || cli_share_exit=$?
e2e_assert_eq "CLI mode allows share --help" "0" "$cli_share_exit"

# CLI mode: serve should be denied (MCP-only command)
cli_serve_exit=0
cli_serve_output=$(AM_INTERFACE_MODE=cli "$MCP_BIN" serve 2>&1) || cli_serve_exit=$?
e2e_assert_eq "CLI mode denies serve (exit 2)" "2" "$cli_serve_exit"

cli_serve_msg=0
echo "$cli_serve_output" | grep -q "not available in CLI mode" || cli_serve_msg=1
e2e_assert_eq "CLI mode denial contains canonical phrase" "0" "$cli_serve_msg"

# CLI mode: config --help should be ALLOWED (config exists in CLI surface)
cli_config_exit=0
AM_INTERFACE_MODE=cli "$MCP_BIN" config --help > /dev/null 2>&1 || cli_config_exit=$?
e2e_assert_eq "CLI mode allows config --help" "0" "$cli_config_exit"

# Invalid AM_INTERFACE_MODE value: should exit 2
invalid_mode_exit=0
AM_INTERFACE_MODE=wat "$MCP_BIN" --help > /dev/null 2>&1 || invalid_mode_exit=$?
e2e_assert_eq "Invalid AM_INTERFACE_MODE exits 2" "2" "$invalid_mode_exit"

# Explicit AM_INTERFACE_MODE=mcp: same as default (denies CLI)
explicit_mcp_exit=0
AM_INTERFACE_MODE=mcp "$MCP_BIN" share > /dev/null 2>&1 || explicit_mcp_exit=$?
e2e_assert_eq "Explicit MCP mode denies share" "2" "$explicit_mcp_exit"

# Case insensitive: CLI/Cli/cLi should all work
for val in CLI Cli cLi; do
    ci_exit=0
    AM_INTERFACE_MODE=$val "$MCP_BIN" --help > /dev/null 2>&1 || ci_exit=$?
    e2e_assert_eq "AM_INTERFACE_MODE=$val accepted" "0" "$ci_exit"
done

# ---------------------------------------------------------------------------
# Summary
# ---------------------------------------------------------------------------

e2e_summary
