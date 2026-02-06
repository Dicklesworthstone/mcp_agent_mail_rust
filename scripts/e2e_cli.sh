#!/usr/bin/env bash
# e2e_cli.sh - E2E CLI stability test suite (br-2ei.9.5)
#
# Tests:
#   1. Top-level --help contains all expected subcommands
#   2. --version outputs semver
#   3. Per-subcommand --help is non-empty and exits 0
#   4. Exit codes for bad arguments (exit 2 from clap)
#   5. JSON output mode: list-projects --json produces parseable JSON
#   6. Commands that require a DB: migrate, list-projects, mail status, acks, file_reservations
#   7. guard status/install in temp repo
#   8. config show-port / set-port
#   9. amctl env output
#  10. Missing subcommand produces non-zero exit

E2E_SUITE="cli"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=e2e_lib.sh
source "${SCRIPT_DIR}/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "CLI Stability E2E Test Suite (br-2ei.9.5)"

# Build both binaries (use >/dev/null so PATH export propagates)
e2e_ensure_binary "am" >/dev/null
e2e_ensure_binary "mcp-agent-mail" >/dev/null

# Ensure cargo debug dir is on PATH (belt-and-suspenders for e2e_ensure_binary)
export PATH="${CARGO_TARGET_DIR}/debug:${PATH}"
e2e_log "am binary: $(command -v am 2>/dev/null || echo NOT_FOUND)"
e2e_log "mcp-agent-mail binary: $(command -v mcp-agent-mail 2>/dev/null || echo NOT_FOUND)"

# Temp workspace for DB-dependent tests
WORK="$(e2e_mktemp "e2e_cli")"
CLI_DB="${WORK}/cli_test.sqlite3"

# ===========================================================================
# Case 1: Top-level --help contains expected subcommands
# ===========================================================================
e2e_case_banner "am --help lists expected subcommands"

HELP_OUT="$(am --help 2>&1)" || true
e2e_save_artifact "case_01_help.txt" "$HELP_OUT"

EXPECTED_CMDS=(
    serve-http serve-stdio lint typecheck share archive guard
    file_reservations acks list-acks migrate list-projects
    clear-and-reset-everything config amctl am-run projects
    mail products docs doctor
)
for cmd in "${EXPECTED_CMDS[@]}"; do
    e2e_assert_contains "help lists '$cmd'" "$HELP_OUT" "$cmd"
done

# ===========================================================================
# Case 2: --version outputs something
# ===========================================================================
e2e_case_banner "am --version exits 0 and produces output"

set +e
VERSION_OUT="$(am --version 2>&1)"
VERSION_RC=$?
set -e

e2e_save_artifact "case_02_version.txt" "$VERSION_OUT"
e2e_assert_exit_code "am --version exits 0" "0" "$VERSION_RC"
# Version should be non-empty
if [ -n "$VERSION_OUT" ]; then
    e2e_pass "version output is non-empty: $VERSION_OUT"
else
    e2e_fail "version output is empty"
fi

# ===========================================================================
# Case 3: mcp-agent-mail --help lists expected subcommands
# ===========================================================================
e2e_case_banner "mcp-agent-mail --help lists expected subcommands"

MCP_HELP="$(mcp-agent-mail --help 2>&1)" || true
e2e_save_artifact "case_03_mcp_help.txt" "$MCP_HELP"

MCP_EXPECTED_CMDS=(serve guard file-reservations acks share archive mail projects products doctor config)
for cmd in "${MCP_EXPECTED_CMDS[@]}"; do
    e2e_assert_contains "mcp help lists '$cmd'" "$MCP_HELP" "$cmd"
done

# ===========================================================================
# Case 4: Per-subcommand --help exits 0 and produces output
# ===========================================================================
e2e_case_banner "Subcommand --help exits 0"

AM_SUBCMDS=(
    share archive guard file_reservations acks config
    amctl projects mail products docs doctor
)
for cmd in "${AM_SUBCMDS[@]}"; do
    set +e
    SUB_HELP="$(am "$cmd" --help 2>&1)"
    SUB_RC=$?
    set -e
    e2e_save_artifact "case_04_help_${cmd}.txt" "$SUB_HELP"
    e2e_assert_exit_code "am $cmd --help" "0" "$SUB_RC"
    if [ -n "$SUB_HELP" ]; then
        e2e_pass "am $cmd --help output is non-empty"
    else
        e2e_fail "am $cmd --help output is empty"
    fi
done

# ===========================================================================
# Case 5: Bad arguments produce exit code 2 (clap error)
# ===========================================================================
e2e_case_banner "Bad arguments exit with code 2"

set +e
am --no-such-flag 2>/dev/null; BAD_FLAG_RC=$?
am serve-http --port not-a-number 2>/dev/null; BAD_PORT_RC=$?
am list-acks 2>/dev/null; MISSING_REQ_RC=$?  # missing required --project --agent
set -e

e2e_assert_exit_code "am --no-such-flag" "2" "$BAD_FLAG_RC"
e2e_assert_exit_code "am serve-http --port not-a-number" "2" "$BAD_PORT_RC"
e2e_assert_exit_code "am list-acks (missing required)" "2" "$MISSING_REQ_RC"

# ===========================================================================
# Case 6: migrate on fresh DB exits 0
# ===========================================================================
e2e_case_banner "migrate on fresh DB exits 0"

set +e
MIGRATE_OUT="$(DATABASE_URL="sqlite:////${CLI_DB}" am migrate 2>&1)"
MIGRATE_RC=$?
set -e

e2e_save_artifact "case_06_migrate.txt" "$MIGRATE_OUT"
e2e_assert_exit_code "am migrate" "0" "$MIGRATE_RC"

# ===========================================================================
# Case 7: list-projects --json on fresh DB produces valid JSON
# ===========================================================================
e2e_case_banner "list-projects --json produces valid JSON"

set +e
LP_OUT="$(DATABASE_URL="sqlite:////${CLI_DB}" am list-projects --json 2>&1)"
LP_RC=$?
set -e

e2e_save_artifact "case_07_list_projects.txt" "$LP_OUT"
e2e_assert_exit_code "am list-projects --json" "0" "$LP_RC"

# Verify it's valid JSON
if echo "$LP_OUT" | python3 -m json.tool >/dev/null 2>&1; then
    e2e_pass "list-projects --json is valid JSON"
else
    e2e_fail "list-projects --json is NOT valid JSON"
    echo "    output: $LP_OUT"
fi

# ===========================================================================
# Case 8: config show-port exits 0
# ===========================================================================
e2e_case_banner "config show-port exits 0"

set +e
SP_OUT="$(DATABASE_URL="sqlite:////${CLI_DB}" am config show-port 2>&1)"
SP_RC=$?
set -e

e2e_save_artifact "case_08_config_show_port.txt" "$SP_OUT"
e2e_assert_exit_code "am config show-port" "0" "$SP_RC"

# ===========================================================================
# Case 9: config set-port + show-port roundtrip
# ===========================================================================
e2e_case_banner "config set-port + show-port roundtrip"

# Use a temp .env file so we don't clobber project's .env
WORK_ENV="${WORK}/.env"
set +e
DATABASE_URL="sqlite:////${CLI_DB}" am config set-port 9999 --env-file "$WORK_ENV" 2>&1
SET_RC=$?
set -e

e2e_assert_exit_code "am config set-port 9999" "0" "$SET_RC"

# Verify the .env file was written with the port
if [ -f "$WORK_ENV" ]; then
    ENV_CONTENT="$(cat "$WORK_ENV")"
    e2e_save_artifact "case_09_env_file.txt" "$ENV_CONTENT"
    e2e_assert_contains ".env contains 9999" "$ENV_CONTENT" "9999"
else
    e2e_fail ".env file not created by set-port"
fi

# ===========================================================================
# Case 10: amctl env exits 0 and produces output
# ===========================================================================
e2e_case_banner "amctl env exits 0"

set +e
AMCTL_OUT="$(DATABASE_URL="sqlite:////${CLI_DB}" am amctl env 2>&1)"
AMCTL_RC=$?
set -e

e2e_save_artifact "case_10_amctl_env.txt" "$AMCTL_OUT"
e2e_assert_exit_code "am amctl env" "0" "$AMCTL_RC"
if [ -n "$AMCTL_OUT" ]; then
    e2e_pass "amctl env output is non-empty"
else
    e2e_fail "amctl env output is empty"
fi

# ===========================================================================
# Case 11: mail status on fresh DB
# ===========================================================================
e2e_case_banner "mail status on fresh DB exits 0"

set +e
MAIL_OUT="$(DATABASE_URL="sqlite:////${CLI_DB}" am mail status /tmp/test_project 2>&1)"
MAIL_RC=$?
set -e

e2e_save_artifact "case_11_mail_status.txt" "$MAIL_OUT"
e2e_assert_exit_code "am mail status" "0" "$MAIL_RC"

# ===========================================================================
# Case 12: acks pending/overdue on fresh DB
# ===========================================================================
e2e_case_banner "acks pending/overdue on fresh DB"

set +e
ACKS_PEND="$(DATABASE_URL="sqlite:////${CLI_DB}" am acks pending /tmp/test TestAgent 2>&1)"
ACKS_P_RC=$?
ACKS_OVER="$(DATABASE_URL="sqlite:////${CLI_DB}" am acks overdue /tmp/test TestAgent 2>&1)"
ACKS_O_RC=$?
set -e

e2e_save_artifact "case_12_acks_pending.txt" "$ACKS_PEND"
e2e_save_artifact "case_12_acks_overdue.txt" "$ACKS_OVER"
e2e_assert_exit_code "am acks pending" "0" "$ACKS_P_RC"
e2e_assert_exit_code "am acks overdue" "0" "$ACKS_O_RC"

# ===========================================================================
# Case 13: file_reservations list/active/soon on fresh DB
# ===========================================================================
e2e_case_banner "file_reservations subcommands on fresh DB"

set +e
FR_LIST="$(DATABASE_URL="sqlite:////${CLI_DB}" am file_reservations list /tmp/test 2>&1)"
FR_L_RC=$?
FR_ACTIVE="$(DATABASE_URL="sqlite:////${CLI_DB}" am file_reservations active /tmp/test 2>&1)"
FR_A_RC=$?
FR_SOON="$(DATABASE_URL="sqlite:////${CLI_DB}" am file_reservations soon /tmp/test 2>&1)"
FR_S_RC=$?
set -e

e2e_save_artifact "case_13_fr_list.txt" "$FR_LIST"
e2e_save_artifact "case_13_fr_active.txt" "$FR_ACTIVE"
e2e_save_artifact "case_13_fr_soon.txt" "$FR_SOON"
e2e_assert_exit_code "am file_reservations list" "0" "$FR_L_RC"
e2e_assert_exit_code "am file_reservations active" "0" "$FR_A_RC"
e2e_assert_exit_code "am file_reservations soon" "0" "$FR_S_RC"

# ===========================================================================
# Case 14: guard status in temp git repo
# ===========================================================================
e2e_case_banner "guard status in temp git repo"

GUARD_REPO="${WORK}/guard_repo"
mkdir -p "$GUARD_REPO"
e2e_init_git_repo "$GUARD_REPO"
echo "init" > "$GUARD_REPO/README.md"
e2e_git_commit "$GUARD_REPO" "initial"

set +e
GS_OUT="$(DATABASE_URL="sqlite:////${CLI_DB}" am guard status "$GUARD_REPO" 2>&1)"
GS_RC=$?
set -e

e2e_save_artifact "case_14_guard_status.txt" "$GS_OUT"
# Guard status should exit cleanly even if not installed
if [ "$GS_RC" -eq 0 ] || [ "$GS_RC" -eq 1 ]; then
    e2e_pass "am guard status exits with 0 or 1 (rc=$GS_RC)"
else
    e2e_fail "am guard status unexpected exit code: $GS_RC"
fi

# ===========================================================================
# Case 15: share subcommand --help texts
# ===========================================================================
e2e_case_banner "share subcommands --help"

SHARE_SUBS=(export update preview verify decrypt wizard)
for sub in "${SHARE_SUBS[@]}"; do
    set +e
    SHARE_HELP="$(am share "$sub" --help 2>&1)"
    SHARE_RC=$?
    set -e
    e2e_save_artifact "case_15_share_${sub}_help.txt" "$SHARE_HELP"
    e2e_assert_exit_code "am share $sub --help" "0" "$SHARE_RC"
done

# ===========================================================================
# Case 16: doctor subcommands --help
# ===========================================================================
e2e_case_banner "doctor subcommands --help"

DOC_SUBS=(check repair backups restore)
for sub in "${DOC_SUBS[@]}"; do
    set +e
    DOC_HELP="$(am doctor "$sub" --help 2>&1)"
    DOC_RC=$?
    set -e
    e2e_save_artifact "case_16_doctor_${sub}_help.txt" "$DOC_HELP"
    e2e_assert_exit_code "am doctor $sub --help" "0" "$DOC_RC"
done

# ===========================================================================
# Case 17: products subcommands --help
# ===========================================================================
e2e_case_banner "products subcommands --help"

PROD_SUBS=(ensure link status search inbox summarize-thread)
for sub in "${PROD_SUBS[@]}"; do
    set +e
    PROD_HELP="$(am products "$sub" --help 2>&1)"
    PROD_RC=$?
    set -e
    e2e_save_artifact "case_17_products_${sub}_help.txt" "$PROD_HELP"
    e2e_assert_exit_code "am products $sub --help" "0" "$PROD_RC"
done

# ===========================================================================
# Case 18: projects subcommands --help
# ===========================================================================
e2e_case_banner "projects subcommands --help"

PROJ_SUBS=(mark-identity discovery-init adopt)
for sub in "${PROJ_SUBS[@]}"; do
    set +e
    PROJ_HELP="$(am projects "$sub" --help 2>&1)"
    PROJ_RC=$?
    set -e
    e2e_save_artifact "case_18_projects_${sub}_help.txt" "$PROJ_HELP"
    e2e_assert_exit_code "am projects $sub --help" "0" "$PROJ_RC"
done

# ===========================================================================
# Case 19: docs insert-blurbs --help
# ===========================================================================
e2e_case_banner "docs insert-blurbs --help"

set +e
DOCS_HELP="$(am docs insert-blurbs --help 2>&1)"
DOCS_RC=$?
set -e

e2e_save_artifact "case_19_docs_help.txt" "$DOCS_HELP"
e2e_assert_exit_code "am docs insert-blurbs --help" "0" "$DOCS_RC"

# ===========================================================================
# Case 20: mcp-agent-mail serve --help exits 0
# ===========================================================================
e2e_case_banner "mcp-agent-mail serve --help"

set +e
SERVE_HELP="$(mcp-agent-mail serve --help 2>&1)"
SERVE_RC=$?
set -e

e2e_save_artifact "case_20_serve_help.txt" "$SERVE_HELP"
e2e_assert_exit_code "mcp-agent-mail serve --help" "0" "$SERVE_RC"
e2e_assert_contains "serve help shows --host" "$SERVE_HELP" "--host"
e2e_assert_contains "serve help shows --port" "$SERVE_HELP" "--port"

# ===========================================================================
# Summary
# ===========================================================================
e2e_summary
