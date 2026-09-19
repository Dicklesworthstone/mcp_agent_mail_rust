#!/usr/bin/env bash
# Exit-status regressions for GH#327. Run with the host's stock Bash.
# --unit needs no network or release binaries. The default also exercises the
# complete signed-release installer; unit fault injection alone cannot prove
# that a real installed release succeeds through its final EXIT trap.
set -euo pipefail

case "${1:-}" in
    ''|--unit) ;;
    *) echo "Usage: $0 [--unit]" >&2; exit 2 ;;
esac

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="${AM_INSTALLER_EXIT_SOURCE:-$REPO_ROOT/install.sh}"
VERSION="${AM_INSTALLER_EXIT_VERSION:-v0.3.36}"
ARTIFACT_DIR="${AM_INSTALLER_EXIT_ARTIFACT_DIR:-$REPO_ROOT/tests/artifacts/installer-exit}"
mkdir -p "$ARTIFACT_DIR"
ARTIFACT_DIR="$(cd "$ARTIFACT_DIR" && pwd -P)"
[ -f "$INSTALL_SH" ] || { echo "Missing installer: $INSTALL_SH" >&2; exit 2; }

# Resolve macOS /var and /tmp aliases before giving paths to the installer.
# Its anti-symlink destination checks must remain enabled, not be worked around
# with --no-verify or a mocked path validator.
scratch="$(mktemp -d)"
scratch="$(cd "$scratch" && pwd -P)"
printf 'Bash: %s\nScratch: %s\nVersion: %s\n' "$BASH_VERSION" "$scratch" "$VERSION"
failures=0

extract_function() {
    awk -v fn="$1" '
        $0 == fn "() {" { found = 1; in_fn = 1 }
        in_fn { print; if ($0 == "}") { complete = 1; exit } }
        END { if (!found || !complete) exit 1 }
    ' "$INSTALL_SH"
}

unit_library="$scratch/exit-functions.sh"
for fn in info ok warn err error_support_hint init_verbose_log verbose \
    dump_verbose_tail on_error early_exit_dump installer_path_owner_uid \
    remove_installer_tmp_dir remove_installer_lock_dir cleanup \
    handle_binary_transaction_signal update_mcp_configs configure_mcp_clients \
    configure_mcp_clients_for_install; do
    extract_function "$fn" >> "$unit_library" \
        || { echo "Cannot extract installer function: $fn" >&2; exit 2; }
done

unit_probe="$scratch/exit-probe.sh"
cat > "$unit_probe" <<'PROBE'
#!/usr/bin/env bash
set -Eeuo pipefail
mode="$1"
root="$2"
source "$3"
QUIET=0 VERBOSE=0 HAS_GUM=0 NO_GUM=1 DRY_RUN=0
LOG_INITIALIZED=0 ERROR_TAIL_EMITTED=0 VERBOSE_DUMP_LINES=20
INSTALLER_EXIT_SUCCESS=0
LOG_FILE="$root/verbose.log"
ISSUES_URL=https://example.invalid/installer-exit-test
BINARY_TRANSACTION_ACTIVE_INSTALL_DIR=''
BINARY_TRANSACTION_RECOVERY_ACTIVE=0
BINARY_TRANSACTION_EXIT_RECOVERY_ATTEMPTED=0
TMP="$root/mcp-agent-mail-install.fixture"
LOCK_DIR="$root/lock.d"
LOCKED=1
mkdir "$TMP" "$LOCK_DIR"
printf '%s\n' "$$" > "$LOCK_DIR/pid"
printf 'owned temporary data\n' > "$TMP/payload"

# The recovery boundary is fault-injected. Normal temp/lock cleanup and the
# complete ERR/EXIT handlers are production functions, running under -Ee.
recover_binary_pair_transaction() {
    printf 'recovery\n' >> "$root/recovery.calls"
    return "${RECOVERY_RC:-0}"
}
trap 'on_error $LINENO' ERR
trap cleanup EXIT
trap 'handle_binary_transaction_signal TERM 143' TERM
fail_required_step() { return 37; }

case "$mode" in
    noop) TMP='' LOCKED=0 ;;
    explicit-success) INSTALLER_EXIT_SUCCESS=1; exit 0 ;;
    incomplete-success) exit 0 ;;
    quiet-success) QUIET=1 VERBOSE=1 ;;
    deliberate-one) exit 1 ;;
    deliberate-23) exit 23 ;;
    unexpected) fail_required_step ;;
    pipefail) fail_required_step | cat ;;
    nounset) printf '%s' "$MISSING_INSTALLER_VARIABLE" ;;
    early-nounset) trap early_exit_dump EXIT; printf '%s' "$MISSING_INSTALLER_VARIABLE" ;;
    guarded) if fail_required_step; then exit 99; fi ;;
    cleanup-fails-success|cleanup-fails-23)
        remove_installer_tmp_dir() { printf 'temp\n' >> "$root/cleanup.calls"; return 71; }
        remove_installer_lock_dir() { printf 'lock\n' >> "$root/cleanup.calls"; return 72; }
        if [ "$mode" = cleanup-fails-23 ]; then exit 23; fi
        ;;
    recovery-fails-success|recovery-fails-23|recovery-succeeds-23)
        BINARY_TRANSACTION_ACTIVE_INSTALL_DIR="$root/journal"
        RECOVERY_RC=71
        if [ "$mode" = recovery-succeeds-23 ]; then RECOVERY_RC=0; fi
        if [ "$mode" != recovery-fails-success ]; then exit 23; fi
        ;;
    term|term-recovery-fails)
        BINARY_TRANSACTION_ACTIVE_INSTALL_DIR="$root/journal"
        if [ "$mode" = term-recovery-fails ]; then RECOVERY_RC=71; fi
        kill -s TERM "$$"
        exit 99
        ;;
    no-agents|required-client|authority-error)
        # Native CLI output is an explicit fixture, not release evidence.
        # Exercise the real setup admission/orchestration functions: no agents
        # is optional, but a discovered client's missing credential is fatal.
        rust_config_env_path() { printf '%s/config.env' "$root"; }
        token_env_targets_outside_git_worktrees() { return 0; }
        resolve_setup_http_bearer_token() { return 0; }
        remote_http_client_target_tools() {
            case "$mode" in
                required-client) printf 'codex\n' ;;
                authority-error) return 42 ;;
            esac
        }
        setup_mcp_configs() { printf 'fallback\n' >> "$root/forbidden.calls"; return 0; }
        sync_codex_http_configs() { printf 'sync\n' >> "$root/forbidden.calls"; return 0; }
        cat > "$root/am" <<'CLI'
#!/bin/sh
case "$*" in
    'setup --help') exit 0 ;;
    'setup run --yes --no-hooks')
        printf 'No coding agents detected. Use --agent to specify agents manually.\n'
        exit 0 ;;
    *) exit 93 ;;
esac
CLI
        chmod 755 "$root/am"
        if ! configure_mcp_clients_for_install "$root/server" "$root/am"; then
            err 'MCP client configuration failed.'
            exit 1
        fi
        printf 'continued\n' > "$root/after-mcp"
        ;;
esac
# Exercise natural EOF after the production tail's false compatibility branch.
if [ 0 -eq 1 ]; then :; fi
INSTALLER_EXIT_SUCCESS=1
PROBE

unit_cases=0
while read -r name expected_rc expected_errs; do
    case_root="$scratch/unit-$name"
    mkdir -p "$case_root/home"
    output="$ARTIFACT_DIR/unit-$name.log"
    rc=0
    env -i PATH="$PATH" HOME="$case_root/home" LC_ALL=C \
        "$BASH" "$unit_probe" "$name" "$case_root" "$unit_library" > "$output" 2>&1 || rc=$?
    errs=$(grep -c 'Unexpected installer error' "$output" || true)
    case_failed=0
    [ "$rc" -eq "$expected_rc" ] && [ "$errs" -eq "$expected_errs" ] || case_failed=1
    case "$name" in
        noop|cleanup-fails-*|early-nounset) ;;
        *) [ ! -e "$case_root/lock.d" ] && [ ! -e "$case_root/mcp-agent-mail-install.fixture" ] || case_failed=1 ;;
    esac
    case "$name" in
        recovery-*|term*)
            [ "$(cat "$case_root/recovery.calls")" = recovery ] || case_failed=1 ;;
        *) [ ! -e "$case_root/recovery.calls" ] || case_failed=1 ;;
    esac
    case "$name" in
        cleanup-fails-*)
            [ "$(cat "$case_root/cleanup.calls")" = "$(printf 'temp\nlock')" ] || case_failed=1 ;;
        unexpected|pipefail)
            if grep -q 'at line 1$' "$output"; then case_failed=1; fi ;;
        no-agents|required-client)
            grep -Fq 'update_mcp_configs:result rc=0' "$case_root/verbose.log" || case_failed=1
            grep -Fq 'update_mcp_configs:output No coding agents detected.' "$case_root/verbose.log" || case_failed=1
            [ ! -e "$case_root/forbidden.calls" ] || case_failed=1
            if [ "$name" = no-agents ]; then
                [ -f "$case_root/after-mcp" ] || case_failed=1
            else
                [ ! -e "$case_root/after-mcp" ] || case_failed=1
                grep -Fq 'Detected remote MCP client setup failed' "$output" || case_failed=1
            fi ;;
        authority-error)
            [ ! -e "$case_root/forbidden.calls" ] && [ ! -e "$case_root/after-mcp" ] || case_failed=1
            grep -Fq 'MCP client authority discovery failed' "$output" || case_failed=1 ;;
    esac
    unit_cases=$((unit_cases + 1))
    printf 'unit %-26s exit=%s expected=%s ERR=%s expected=%s\n' "$name" "$rc" "$expected_rc" "$errs" "$expected_errs"
    if [ "$case_failed" -ne 0 ]; then
        failures=$((failures + 1))
        cat "$output" >&2
    fi
done <<'CASES'
noop 0 0
success 0 0
explicit-success 0 0
incomplete-success 1 0
quiet-success 0 0
deliberate-one 1 0
deliberate-23 23 0
unexpected 37 1
pipefail 37 1
nounset 1 0
early-nounset 1 0
guarded 0 0
cleanup-fails-success 0 0
cleanup-fails-23 23 0
recovery-fails-success 1 0
recovery-fails-23 23 0
recovery-succeeds-23 23 0
term 143 0
term-recovery-fails 143 0
no-agents 0 0
required-client 1 0
authority-error 1 0
CASES

printf 'Unit exit regressions: %s cases, %s failure(s)\n' "$unit_cases" "$failures"
if [ "${1:-}" = --unit ]; then
    [ "$failures" -eq 0 ]
    exit $?
fi
[ "$failures" -eq 0 ] || exit 1
minisign_bin=$(command -v minisign) || { echo 'The signed-release tests require minisign; use --unit for offline coverage.' >&2; exit 2; }
# A clean HOME is insufficient when PATH still exposes the caller's Codex or
# OMP binary: their presence intentionally requires successful client setup.
# Keep real system tools and the real signature verifier, but no ambient agents.
mkdir "$scratch/bin"
ln -s "$minisign_bin" "$scratch/bin/minisign"
install_test_path="$scratch/bin:/usr/bin:/bin:/usr/sbin:/sbin"

for mode in file stdin; do
    case_root="$scratch/$mode"
    mkdir -p "$case_root/home" "$case_root/work" "$case_root/tmp"
    trace="$ARTIFACT_DIR/$mode.trace.log"
    verbose_log="$ARTIFACT_DIR/$mode.verbose.log"
    rc=0
    (
        cd "$case_root/work"
        # No caller credentials, XDG locations, shell startup hooks, or installer
        # skip overrides may leak into the clean-HOME reproduction.
        if [ "$mode" = file ]; then
            env -i HOME="$case_root/home" PATH="$install_test_path" SHELL=/bin/bash \
                TMPDIR="$case_root/tmp" LOG_FILE="$verbose_log" LC_ALL=C TERM=dumb \
                "$BASH" -x "$INSTALL_SH" --version "$VERSION" --yes --no-gum --no-service --verbose
        else
            env -i HOME="$case_root/home" PATH="$install_test_path" SHELL=/bin/bash \
                TMPDIR="$case_root/tmp" LOG_FILE="$verbose_log" LC_ALL=C TERM=dumb \
                "$BASH" -x -s -- --version "$VERSION" --yes --no-gum --no-service --verbose < "$INSTALL_SH"
        fi
    ) > "$trace" 2>&1 || rc=$?
    printf '%s installer exit=%s\n' "$mode" "$rc" | tee "$ARTIFACT_DIR/$mode.status.txt"
    if [ "$rc" -ne 0 ]; then
        failures=$((failures + 1))
        tail -n 100 "$trace" >&2
    fi
    # Inspect the installed binaries even on failure: GH#327 specifically had
    # working, authenticated binaries and an erroneous installer exit status.
    for binary in am mcp-agent-mail; do
        installed="$case_root/home/.local/bin/$binary"
        if [ -x "$installed" ] && [ "$("$installed" --version)" = "$binary ${VERSION#v}" ]; then
            printf '%s %s: exact installed version verified\n' "$mode" "$binary"
        else
            printf '%s %s: missing or wrong installed version\n' "$mode" "$binary" >&2
            failures=$((failures + 1))
        fi
    done
    for witness in 'verify_minisign:ok' 'verify_checksum:ok' \
        'update_mcp_configs:result rc=0' \
        'update_mcp_configs:output No coding agents detected.'; do
        if ! grep -Fq "$witness" "$verbose_log"; then
            printf '%s: missing real-path witness: %s\n' "$mode" "$witness" >&2
            failures=$((failures + 1))
        fi
    done
    if grep -q '^++* on_error ' "$trace"; then
        echo "$mode: ERR handler ran on the successful-install path" >&2
        failures=$((failures + 1))
    fi
done
printf 'Installer exit regression: %s failure(s); evidence: %s\n' "$failures" "$ARTIFACT_DIR"
[ "$failures" -eq 0 ]
