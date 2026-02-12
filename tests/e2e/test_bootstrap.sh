#!/usr/bin/env bash
# test_bootstrap.sh - E2E suite for command-surface sanity checks.
#
# This suite guards the startup contract that should catch dependency/API regressions
# that would otherwise break CLI availability and CLI/MCP dual-mode launch paths.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export E2E_SUITE="bootstrap"

# shellcheck source=e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Bootstrap Surface E2E Suite"

# Build both binaries (if not already present)
e2e_ensure_binary "mcp-agent-mail" >/dev/null
e2e_ensure_binary "am" >/dev/null
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"

# ===========================================================================
# Case 1: MCP binary help text includes expected server command surfaces
# ===========================================================================
e2e_case_banner "mcp-agent-mail --help lists server entry points"

set +e
MCP_HELP_OUT="$(mcp-agent-mail --help 2>&1)"
MCP_HELP_RC=$?
set -e

e2e_save_artifact "case_01_mcp_help.txt" "$MCP_HELP_OUT"
e2e_assert_exit_code "mcp-agent-mail --help" "0" "$MCP_HELP_RC"
e2e_assert_contains "mcp help mentions serve" "$MCP_HELP_OUT" "serve"
e2e_assert_contains "mcp help mentions config" "$MCP_HELP_OUT" "config"
e2e_assert_contains "mcp help points to CLI" "$MCP_HELP_OUT" "am --help"

# ===========================================================================
# Case 2: MCP server help exits cleanly and documents key options
# ===========================================================================
e2e_case_banner "mcp-agent-mail serve --help is stable"

set +e
MCP_SERVE_HELP_OUT="$(mcp-agent-mail serve --help 2>&1)"
MCP_SERVE_HELP_RC=$?
set -e

e2e_save_artifact "case_02_mcp_serve_help.txt" "$MCP_SERVE_HELP_OUT"
e2e_assert_exit_code "mcp-agent-mail serve --help" "0" "$MCP_SERVE_HELP_RC"
e2e_assert_contains "serve help shows host option" "$MCP_SERVE_HELP_OUT" "--host"
e2e_assert_contains "serve help shows no-tui option" "$MCP_SERVE_HELP_OUT" "--no-tui"

# ===========================================================================
# Case 3: MCP config command succeeds and returns a Config payload
# ===========================================================================
e2e_case_banner "mcp-agent-mail config emits config structure"

set +e
MCP_CONFIG_OUT="$(mcp-agent-mail config 2>&1)"
MCP_CONFIG_RC=$?
set -e

e2e_save_artifact "case_03_mcp_config.txt" "$MCP_CONFIG_OUT"
e2e_assert_exit_code "mcp-agent-mail config" "0" "$MCP_CONFIG_RC"
e2e_assert_contains "config output includes Config" "$MCP_CONFIG_OUT" "Config {"
e2e_assert_contains "config output includes http_host" "$MCP_CONFIG_OUT" "http_host:"
e2e_assert_contains "config output includes log_level" "$MCP_CONFIG_OUT" "log_level:"

# ===========================================================================
# Case 4: CLI help is present and includes known subcommands
# ===========================================================================
e2e_case_banner "am --help lists core command families"

set +e
AM_HELP_OUT="$(am --help 2>&1)"
AM_HELP_RC=$?
set -e

e2e_save_artifact "case_04_am_help.txt" "$AM_HELP_OUT"
e2e_assert_exit_code "am --help" "0" "$AM_HELP_RC"
e2e_assert_contains "am help includes guard command" "$AM_HELP_OUT" "guard"
e2e_assert_contains "am help includes mail command" "$AM_HELP_OUT" "mail"
e2e_assert_contains "am help includes share command" "$AM_HELP_OUT" "share"
