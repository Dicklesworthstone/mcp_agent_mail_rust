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
    installer_path_mode ensure_real_directory_tree \
    installer_directory_identity installer_lock_fd_identity installer_kernel_lock \
    acquire_installer_lock verify_installer_lock_authorities release_installer_lock \
    remove_installer_tmp_dir cleanup \
    capture_command_with_timeout installed_am_db_query_scalar installed_am_db_exec \
    installed_am_db_backup ensure_remote_http_client_readiness activate_mac_python_cli_compat_shell \
    handle_binary_transaction_signal ensure_rust update_mcp_configs configure_mcp_clients \
    configure_mcp_clients_for_install path_resolves_within_directory \
    config_target_is_hardlink_aliased mcp_config_must_skip_shell_write \
    setup_claude_code_mcp_via_cli setup_mcp_configs resolve_setup_http_bearer_token; do
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
LOCKED=0 DESTINATION_LOCKED=0
mkdir "$TMP" "$LOCK_DIR"
printf '%s\n' "$$" > "$LOCK_DIR/pid"
acquire_installer_lock "$LOCK_DIR" initial
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
        release_installer_lock() {
            [ "$1" = initial ] || return 0
            printf 'lock\n' >> "$root/cleanup.calls"; return 72
        }
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
    missing-nightly)
        # Actual command discovery sees an empty PATH. A source install must
        # fail here, before attempting an unchecked toolchain bootstrap.
        mkdir "$root/no-toolchain"
        if PATH="$root/no-toolchain" ensure_rust "$root"; then
            printf 'source build continued without a compiler\n' > "$root/forbidden.calls"
            exit 99
        fi
        exit 1
        ;;
    source-nightly|source-stable|source-missing)
        # Compiler output is an explicit R2 fixture. This exercises the real
        # context selection without requiring Rustup or downloading a compiler.
        # Real installed-toolchain controls compensate for this fixture lane.
        source_root="$root/tagged-source"
        if [ "$mode" != source-missing ]; then mkdir "$source_root"; fi
        original_cwd=$(pwd -P)
        cargo() { printf 'unexpected cargo execution\n' > "$root/forbidden.calls"; return 97; }
        rustc() {
            pwd -P > "$root/rustc.cwd"
            if [ "$PWD" = "$source_root" ] && [ "$mode" = source-nightly ]; then
                printf 'rustc 1.100.0-nightly (explicit fixture)\n'
            else
                printf 'rustc 1.99.0 (explicit fixture)\n'
            fi
        }
        toolchain_rc=0
        ensure_rust "$source_root" || toolchain_rc=$?
        [ "$(pwd -P)" = "$original_cwd" ] || exit 99
        if [ "$toolchain_rc" -eq 0 ]; then
            [ "$mode" = source-nightly ] || exit 99
        else
            [ "$mode" != source-nightly ] || exit 99
            exit 1
        fi
        printf 'continued\n' > "$root/after-toolchain"
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
if [ -e /dev/fd/7 ] || [ -e /dev/fd/8 ]; then exit 94; fi
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
# ERR inheritance, LINENO, and EXIT handling must also work when Bash reads
# the program from stdin. Use a fresh Bash, not a probe function sourced in a
# conditional: the latter would disable errexit inside its shell functions.
for delivery in file stdin; do
while read -r name expected_rc expected_errs; do
    case_root="$scratch/unit-$delivery-$name"
    mkdir -p "$case_root/home"
    output="$ARTIFACT_DIR/unit-$delivery-$name.log"
    rc=0
    if [ "$delivery" = file ]; then
        env -i PATH="$PATH" HOME="$case_root/home" LC_ALL=C \
            "$BASH" "$unit_probe" "$name" "$case_root" "$unit_library" > "$output" 2>&1 || rc=$?
    else
        env -i PATH="$PATH" HOME="$case_root/home" LC_ALL=C \
            "$BASH" -s -- "$name" "$case_root" "$unit_library" < "$unit_probe" > "$output" 2>&1 || rc=$?
    fi
    errs=$(grep -c 'Unexpected installer error' "$output" || true)
    case_failed=0
    [ "$rc" -eq "$expected_rc" ] && [ "$errs" -eq "$expected_errs" ] || case_failed=1
    case "$name" in
        noop|cleanup-fails-*|early-nounset) ;;
        *) [ -d "$case_root/lock.d" ] && [ -f "$case_root/lock.d/pid" ] && \
           [ ! -e "$case_root/mcp-agent-mail-install.fixture" ] || case_failed=1 ;;
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
        missing-nightly)
            [ ! -e "$case_root/forbidden.calls" ] || case_failed=1
            grep -Fq 'requires an installed Rust nightly toolchain' "$output" || case_failed=1 ;;
        source-nightly|source-stable|source-missing)
            [ ! -e "$case_root/forbidden.calls" ] || case_failed=1
            if [ "$name" = source-missing ]; then
                [ ! -e "$case_root/rustc.cwd" ] || case_failed=1
            else
                [ "$(cat "$case_root/rustc.cwd")" = "$case_root/tagged-source" ] || case_failed=1
            fi
            if [ "$name" = source-nightly ]; then
                [ -f "$case_root/after-toolchain" ] || case_failed=1
            else
                [ ! -e "$case_root/after-toolchain" ] || case_failed=1
                grep -Fq 'requires an installed Rust nightly toolchain' "$output" || case_failed=1
            fi ;;
    esac
    unit_cases=$((unit_cases + 1))
    printf 'unit %-5s %-26s exit=%s expected=%s ERR=%s expected=%s\n' "$delivery" "$name" "$rc" "$expected_rc" "$errs" "$expected_errs"
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
missing-nightly 1 0
source-nightly 0 0
source-stable 1 0
source-missing 1 0
CASES
done

# A regular-file destination must be refused before any lock/state write.
# Leftover PID records in the old global location are preserved, including a
# PID that is alive again: they are no longer permission to remove or block it.
lock_cases=0
for delivery in file stdin; do
for location in tmpdir trailing-slash explicit empty-override; do
for ownership in free busy; do
    case_root="$scratch/lock-$delivery-$location-$ownership"
    mkdir -p "$case_root/home" "$case_root/work" "$case_root/tmp root" "$case_root/lock root"
    printf 'destination sentinel\n' > "$case_root/destination"
    tmp_parent="$case_root/tmp root"
    [ "$location" != trailing-slash ] || tmp_parent="$tmp_parent/"
    expected_lock="$tmp_parent/mcp-agent-mail-install.lock"
    lock_env=("PATH=/usr/bin:/bin:/usr/sbin:/sbin" "HOME=$case_root/home"
        "TMPDIR=$tmp_parent" "LOG_FILE=$case_root/installer.log" "LC_ALL=C" "TERM=dumb")
    case "$location" in
        explicit)
            expected_lock="$case_root/lock root/selected.lock"
            lock_env+=("AM_INSTALL_LOCK_FILE=$expected_lock")
            ;;
        empty-override) lock_env+=("AM_INSTALL_LOCK_FILE=") ;;
    esac
    if [ "$ownership" = busy ]; then
        mkdir "${expected_lock}.d"
        printf '%s\n' "$$" > "${expected_lock}.d/pid"
    fi
    installer_args=(--version "$VERSION" --yes --no-gum --no-easy --no-service
        --offline --force --dest "$case_root/destination")
    output="$ARTIFACT_DIR/lock-$delivery-$location-$ownership.log"
    rc=0
    (
        cd "$case_root/work" || exit
        if [ "$delivery" = file ]; then
            env -i "${lock_env[@]}" "$BASH" -x "$INSTALL_SH" "${installer_args[@]}" < /dev/null
        else
            env -i "${lock_env[@]}" "$BASH" -x -s -- "${installer_args[@]}" < "$INSTALL_SH"
        fi
    ) > "$output" 2>&1 || rc=$?
    case_failed=0
    [ "$rc" -eq 1 ] || case_failed=1
    [ "$(cat "$case_root/destination")" = 'destination sentinel' ] || case_failed=1
    grep -Fq 'Install destination contains traversal, a symlink, or a non-directory component' "$output" || case_failed=1
    if grep -Fq '+ LOCKED=1' "$output" || grep -Fq '+ DESTINATION_LOCKED=1' "$output"; then case_failed=1; fi
    if [ "$ownership" = busy ]; then
        [ "$(cat "${expected_lock}.d/pid")" = "$$" ] || case_failed=1
    else
        [ ! -e "${expected_lock}.d" ] || case_failed=1
    fi
    if grep -Fq 'Downloading https://' "$output"; then case_failed=1; fi
    lock_cases=$((lock_cases + 1))
    printf 'lock %-5s %-14s %-4s exit=%s expected=1\n' "$delivery" "$location" "$ownership" "$rc"
    if [ "$case_failed" -ne 0 ]; then
        failures=$((failures + 1))
        tail -n 60 "$output" >&2
    fi
done
done
done

# Run two complete installers with distinct temporary roots. The first pauses
# at the download boundary while holding its real locks. Network substitution
# controls that interleaving only; the signed installations below remain the
# real dependency positives. A shared destination must refuse the contender
# before recovery, while unrelated destinations can make progress.
destination_cases=0
for delivery in file stdin; do
for sharing in same different shared-tmp alias public-inode; do
    if ! python3 - "$INSTALL_SH" "$scratch/destination-$delivery-$sharing" \
        "$ARTIFACT_DIR" "$BASH" "$VERSION" "$delivery" "$sharing" <<'PY'
import fcntl
import os
from pathlib import Path
import subprocess
import sys
import time

source, case_root, artifacts, bash, version, delivery, sharing = sys.argv[1:]
root = Path(case_root)
assert not root.exists(), "concurrency cases need independent temporary roots"
for name in ("bin", "home", "tmp-first", "tmp-second", "destination"):
    (root / name).mkdir(parents=True, exist_ok=True)
curl = root / "bin/curl"
curl.write_text("""#!/bin/bash
for argument; do
  [ "$argument" = -fsSI ] && exit 22
done
printf '%s\\n' "$$" > "$PROBE_ENTRY"
if [ "$PROBE_FIRST" = 1 ]; then
  attempt=0
  while [ "$attempt" -lt 1500 ]; do
    [ -e "$PROBE_RELEASE" ] && exit 22
    attempt=$((attempt + 1))
    sleep 0.02
  done
fi
exit 22
""")
curl.chmod(0o755)
prefix = "destination-" + delivery + "-" + sharing

def assert_locked(path, expected, primary=True):
    if primary:
        path = path / f".mcp-agent-mail-install.lock.{os.geteuid()}.d"
    fd = os.open(path, os.O_RDONLY | os.O_DIRECTORY)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            assert expected, f"unexpected lock survives: {path}"
        else:
            assert not expected, f"live installer lost its lock: {path}"
    finally:
        os.close(fd)

def launch(role, destination):
    env = {
        "HOME": str(root / "home"),
        "PATH": str(root / "bin") + ":/usr/bin:/bin:/usr/sbin:/sbin",
        "SHELL": "/bin/bash", "LC_ALL": "C", "TERM": "dumb",
        "TMPDIR": str(root / ("tmp-first" if sharing == "shared-tmp" else "tmp-" + role)),
        "LOG_FILE": str(Path(artifacts) / (prefix + "-" + role + ".verbose.log")),
        "PROBE_ENTRY": str(root / (role + ".curl")),
        "PROBE_RELEASE": str(root / "release"),
        "PROBE_FIRST": "1" if role == "first" else "0",
    }
    if role == "first" and sharing == "alias":
        # Distinct spellings of one lock directory must not self-contend.
        env["AM_INSTALL_LOCK_FILE"] = str(destination) + f"/./.mcp-agent-mail-install.lock.{os.geteuid()}"
    args = ["--version", version, "--dest", str(destination), "--yes",
            "--no-gum", "--no-service", "--no-migrate"]
    log = Path(artifacts) / (prefix + "-" + role + ".log")
    with log.open("wb") as output:
        if delivery == "file":
            return subprocess.Popen([bash, source, *args], env=env, cwd=root,
                                    stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT)
        with open(source, "rb") as stdin:
            return subprocess.Popen([bash, "-s", "--", *args], env=env, cwd=root,
                                    stdin=stdin, stdout=output, stderr=subprocess.STDOUT)

public_fd = None
if sharing == "public-inode":
    # A world-readable destination is not private lock authority. This real
    # unrelated inode lock must not prevent the installer reaching download.
    public_fd = os.open(root / "destination", os.O_RDONLY | os.O_DIRECTORY)
    fcntl.flock(public_fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
first = launch("first", root / "destination")
try:
    deadline = time.monotonic() + 15
    while not (root / "first.curl").exists() and first.poll() is None and time.monotonic() < deadline:
        time.sleep(0.02)
    first_log = Path(artifacts) / (prefix + "-first.log")
    assert (root / "first.curl").exists(), first_log.read_text()
    destination_lock = root / "destination"
    assert_locked(destination_lock, True)
    initial = destination_lock / f".mcp-agent-mail-install.lock.{os.geteuid()}.d" if sharing == "alias" else None
    if initial is not None:
        assert_locked(initial, True, primary=False)
        assert initial.stat().st_mode & 0o777 == 0o700
    else:
        assert not (root / "tmp-first/mcp-agent-mail-install.lock.d").exists()
    evidence = None
    if delivery == "stdin" and sharing not in ("different", "shared-tmp"):
        journal = root / "destination/.mcp-agent-mail-install-transaction.active"
        journal.mkdir(mode=0o700)
        evidence = journal / "live-owner-evidence"
        evidence.write_bytes(b"leave the active owner's journal unchanged\n")
        before = (evidence.stat().st_ino, evidence.read_bytes())
    second_dest = root / ("other-destination" if sharing in ("different", "shared-tmp") else "destination")
    second = launch("second", second_dest)
    second_rc = second.wait(timeout=15)
    second_log = (Path(artifacts) / (prefix + "-second.log")).read_text()
    assert second_rc == 1, second_log
    assert first.poll() is None, first_log.read_text()
    assert_locked(destination_lock, True)
    if initial is not None:
        assert_locked(initial, True, primary=False)
    if sharing in ("different", "shared-tmp"):
        assert (root / "second.curl").exists(), second_log
        assert "Another installer is running" not in second_log, second_log
        assert_locked(second_dest, False)
    else:
        assert not (root / "second.curl").exists(), second_log
        assert "Another installer is running" in second_log, second_log
        assert "previous binary transaction is ambiguous" not in second_log, second_log
        assert_locked(destination_lock, True)
    assert not (root / "tmp-second/mcp-agent-mail-install.lock.d").exists()
    if evidence is not None:
        assert (evidence.stat().st_ino, evidence.read_bytes()) == before
finally:
    (root / "release").touch()
    first_rc = first.wait(timeout=15)
    if public_fd is not None:
        os.close(public_fd)
assert first_rc == 1, first_log.read_text()
assert_locked(destination_lock, False)
if initial is not None:
    assert_locked(initial, False, primary=False)
    assert not (initial / "pid").exists(), "kernel locking never creates a PID authority"
if evidence is not None:
    assert (evidence.stat().st_ino, evidence.read_bytes()) == before
print("destination", delivery, sharing, "owner preserved; contender and cleanup verified")
PY
    then
        failures=$((failures + 1))
    fi
    destination_cases=$((destination_cases + 1))
done
done

# GH#376: inspect real inherited descriptors, even when a /dev/fd pathname
# would report a different device. Adapter discovery and negative outputs are
# explicit fixtures; the positive Python/Perl/procfs cases use native metadata.
identity_probe="$scratch/fd-identity-probe.sh"
cat > "$identity_probe" <<'PROBE'
#!/usr/bin/env bash
set -Eeuo pipefail
root="$1" backend="$2" mode="$3"
source "$4"
QUIET=0 HAS_GUM=0 NO_GUM=1
LOCKED=0 DESTINATION_LOCKED=0
mkdir "$root/destination"
private="$root/destination/.mcp-agent-mail-install.lock.$(id -u).d"
mkdir -m 700 "$private"
printf 'keep this file\n' > "$root/file"
expected=$(installer_directory_identity "$private")
wrong_device="1${expected%:*}:${expected#*:}"
wrong_inode="${expected%:*}:1${expected#*:}"

command() {
    case "${1:-}:${2:-}" in
        -v:python3) case "$backend" in python|perl-fallback) ;; *) return 1 ;; esac ;;
        -v:perl) case "$backend" in perl|perl-fallback) ;; *) return 1 ;; esac ;;
        -v:flock) [ "$backend" = procfs ] || return 1 ;;
    esac
    builtin command "$@"
}
uname() {
    # On Linux, model Darwin's lack of a trustworthy procfs fallback as well
    # as its bad /dev/fd device. No macOS kernel behavior is claimed here.
    if [ "$backend" = procfs ]; then builtin command uname "$@"; else printf 'Darwin\n'; fi
}
fault_output() {
    case "$mode" in
        device-mismatch) printf '%s' "$wrong_device" ;;
        inode-mismatch) printf '%s' "$wrong_inode" ;;
        empty) return 0 ;;
        malformed) printf 'not-an-identity\n' ;;
        noisy) printf '%s\nunexpected output\n' "$expected" ;;
        nonzero) printf '%s' "$expected"; return 71 ;;
        *) return 99 ;;
    esac
}
native_identity() {
    local tool="$1"
    shift
    printf '%s\n' "$tool" >> "$root/adapters"
    if [ "$backend:$tool" = perl-fallback:python3 ]; then
        # An installed but unusable Python (including a developer-tools shim)
        # must not suppress a working independent native identity provider.
        printf '%s' "$expected"
        return 71
    fi
    case "$mode" in
        device-mismatch|inode-mismatch|empty|malformed|noisy|nonzero) fault_output ;;
        *) builtin command "$tool" "$@" ;;
    esac
}
python3() { native_identity python3 "$@"; }
perl() { native_identity perl "$@"; }
stat() {
    local argument=""
    for argument in "$@"; do
        case "$argument" in
            /dev/fd/*)
                printf 'untrustworthy devfs identity consulted\n' >> "$root/devfs.calls"
                printf '%s' "$wrong_device"
                return 0
                ;;
            /proc/self/fd/*)
                [ "$backend" = procfs ] || return 72
                case "$mode" in
                    device-mismatch|inode-mismatch|empty|malformed|noisy|nonzero) fault_output; return $? ;;
                esac
                ;;
        esac
    done
    builtin command stat "$@"
}

for fd in 7 8; do
    case "$fd" in 7) exec 7< "$private" ;; 8) exec 8< "$private" ;; esac
    case "$mode:$fd" in
        file:7) exec 7< "$root/file" ;; file:8) exec 8< "$root/file" ;;
        closed:7) exec 7<&- ;; closed:8) exec 8<&- ;;
    esac
    rc=0
    identity=$(installer_lock_fd_identity "$fd") || rc=$?
    case "$mode" in
        ordinary) [ "$rc" -eq 0 ] && [ "$identity" = "$expected" ] || exit 93 ;;
        device-mismatch) [ "$rc" -eq 0 ] && [ "$identity" = "$wrong_device" ] || exit 93 ;;
        inode-mismatch) [ "$rc" -eq 0 ] && [ "$identity" = "$wrong_inode" ] || exit 93 ;;
        *) [ "$rc" -ne 0 ] && [ -z "$identity" ] || exit 93 ;;
    esac
    case "$fd" in 7) exec 7<&- ;; 8) exec 8<&- ;; esac
done
for fd in 0 3 9 -1 invalid; do
    rc=0
    identity=$(installer_lock_fd_identity "$fd") || rc=$?
    [ "$rc" -ne 0 ] && [ -z "$identity" ] || exit 93
done

case "$mode:$backend" in
    ordinary:perl-fallback)
        # Identity recovery is independent of the existing kernel adapter's
        # selection policy; prove both native providers were actually tried.
        grep -qx python3 "$root/adapters"
        grep -qx perl "$root/adapters"
        ;;
    ordinary:*)
        acquire_installer_lock "$root/destination" destination
        [ "$DESTINATION_LOCKED" -eq 1 ]
        verify_installer_lock_authorities
        release_installer_lock destination
        ;;
    file:*|closed:*) ;;
    *)
        # Valid-looking but wrong, failed, or missing identities must never
        # authorize acquisition, for either the primary or auxiliary lock.
        if acquire_installer_lock "$root/destination" destination; then exit 91; fi
        [ "$DESTINATION_LOCKED" -eq 0 ]
        if acquire_installer_lock "$private" initial; then exit 92; fi
        [ "$LOCKED" -eq 0 ]
        ;;
esac
[ ! -e "$root/devfs.calls" ]
[ "$(cat "$root/file")" = 'keep this file' ]
printf 'descriptor identity and acquisition checks passed\n'
PROBE

identity_cases=0
for backend in python perl perl-fallback procfs missing; do
    case "$backend" in
        python) command -v python3 >/dev/null || continue ;;
        perl|perl-fallback) command -v perl >/dev/null || continue ;;
        procfs) [ "$(uname -s)" = Linux ] && command -v flock >/dev/null || continue ;;
    esac
    modes='ordinary file closed device-mismatch inode-mismatch empty malformed noisy nonzero'
    case "$backend" in perl-fallback) modes=ordinary ;; missing) modes=missing ;; esac
    for delivery in file stdin; do
    for mode in $modes; do
        case_root="$scratch/identity-$backend-$delivery-$mode"
        mkdir "$case_root"
        output="$ARTIFACT_DIR/identity-$backend-$delivery-$mode.log"
        rc=0
        if [ "$delivery" = file ]; then
            "$BASH" "$identity_probe" "$case_root" "$backend" "$mode" "$unit_library" > "$output" 2>&1 || rc=$?
        else
            "$BASH" -s -- "$case_root" "$backend" "$mode" "$unit_library" < "$identity_probe" > "$output" 2>&1 || rc=$?
        fi
        identity_cases=$((identity_cases + 1))
        printf 'identity %-13s %-5s %-15s exit=%s expected=0\n' "$backend" "$delivery" "$mode" "$rc"
        if [ "$rc" -ne 0 ]; then
            failures=$((failures + 1))
            cat "$output" >&2
        fi
    done
    done
done

# Exercise production lock helpers against real OS locks. Isolate discovery
# only for kernel locking so macOS can still use native descriptor identities.
# Faults and traces distinguish a working fallback from an untested adapter.
kernel_probe="$scratch/kernel-lock-probe.sh"
cat > "$kernel_probe" <<'PROBE'
#!/usr/bin/env bash
set -euo pipefail
root="$1" backend="$2" mode="$3"
source "$4"
QUIET=0 HAS_GUM=0 NO_GUM=1
LOCKED=0 DESTINATION_LOCKED=0
real_flock=$(type -P flock) || real_flock=''
real_python=$(type -P python3) || real_python=''
real_perl=$(type -P perl) || real_perl=''
command() {
    if [ "${FUNCNAME[1]:-}" = installer_kernel_lock ] && [ "${1:-}" = -v ]; then
        case "$backend:${2:-}" in
            native:flock|python:python3|perl:perl|broken-flock:flock|broken-flock:python3|\
            broken-python:python3|broken-python:perl|broken-both:*|failing:*|closed:*) return 0 ;;
            *) return 1 ;;
        esac
    fi
    builtin command "$@" || return "$?"
}
kernel_helper() {
    local helper="$1" in_kernel=0 executable=''
    shift
    if [ "${FUNCNAME[2]:-}" = installer_kernel_lock ]; then
        in_kernel=1
        printf '%s\n' "$helper" >> "$root/kernel.adapters"
    fi
    case "$backend:$helper" in
        broken-flock:flock|broken-both:flock|broken-python:python3|broken-both:python3) return 71 ;;
    esac
    if [ "$in_kernel" -eq 1 ]; then
        case "$backend" in failing) return 71 ;; closed) return 0 ;; esac
    fi
    case "$helper" in flock) executable="$real_flock" ;; python3) executable="$real_python" ;; perl) executable="$real_perl" ;; esac
    [ -n "$executable" ] || return 127
    "$executable" "$@"
}
flock() { kernel_helper flock "$@"; }
python3() { kernel_helper python3 "$@"; }
perl() { kernel_helper perl "$@"; }
printf 'before\nafter\n' > "$root/fd3-sentinel"
exec 3< "$root/fd3-sentinel"
if [ "$mode" = fault ] || [ "$mode" = denied ]; then
    if [ "$backend" = closed ]; then
        exec 7<&-; exec 8<&-
        for fd in 7 8 0 3 9 invalid; do
            for operation in acquire release; do
                if installer_kernel_lock "$fd" "$operation"; then exit 92; fi
            done
        done
    else
        for kind in initial destination; do
            [ "$mode:$kind" != denied:initial ] || continue
            if acquire_installer_lock "${5:-$root/authority}" "$kind"; then exit 92; fi
            [ "$LOCKED:$DESTINATION_LOCKED" = 0:0 ] || exit 93
            case "$kind" in initial) fd=7 ;; destination) fd=8 ;; esac
            if installer_lock_fd_identity "$fd"; then exit 94; fi
        done
    fi
    IFS= read -r witness <&3
    [ "$witness" = before ] || exit 95
    printf 'refused\n' > "$root/refused"
    exit 0
fi
acquire_installer_lock "$root/authority" destination
IFS= read -r witness <&3
[ "$witness" = before ] || exit 95
case "$mode" in
    descendant-*) sleep 30 & printf '%s\n' "$!" > "$root/child" ;;
esac
printf 'ready\n' > "$root/ready"
IFS= read -r action
if [ "$action" = verify ]; then
    # Keep the actual lock alive on fd 9 while replacing the reserved fd.
    # Metadata read from a pathname would miss this substitution entirely.
    case "$mode" in
        descriptor-directory|descriptor-file) exec 9<&8; exec 8< "$root/substitute" ;;
        descriptor-closed) exec 9<&8; exec 8<&- ;;
    esac
    if verify_installer_lock_authorities; then
        echo 'FAIL: a substituted directory passed authority validation' >&2
        exit 91
    fi
    printf 'refused\n' > "$root/refused"
    case "$mode" in descriptor-*) exec 8<&9; exec 9<&- ;; esac
fi
release_installer_lock destination
IFS= read -r witness <&3
[ "$witness" = after ] || exit 95
PROBE

kernel_cases=0
if python3 - "$kernel_probe" "$unit_library" "$scratch/kernel" "$ARTIFACT_DIR" "$BASH" <<'PY'
import fcntl
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import time

probe, library, scratch, artifacts, bash = sys.argv[1:]
completed = 0
unavailable = []

def locked(path, primary=True):
    authority = path / f".mcp-agent-mail-install.lock.{os.geteuid()}.d" if primary else path
    fd = os.open(authority, os.O_RDONLY | os.O_DIRECTORY)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        return False
    finally:
        os.close(fd)

backends = (("native", "flock", ["flock"]), ("python", "python3", ["python3"]),
            ("perl", "perl", ["perl"]), ("broken-flock", "python3", ["flock", "python3"]),
            ("broken-python", "perl", ["python3", "perl"]),
            ("broken-both", "perl", ["flock", "python3", "perl"]))
for backend, executable, adapters in backends:
    if shutil.which(executable) is None:
        unavailable.append(backend)
        continue
    for mode in ("ordinary", "rename", "symlink", "crash", "descendant-normal", "descendant-crash",
                 "precreated", "precreated-public", "precreated-symlink", "precreated-file", "precreated-fifo",
                 "authority-rename", "authority-symlink", "authority-permissions",
                 "descriptor-directory", "descriptor-file", "descriptor-closed"):
        root = Path(scratch) / (backend + "-" + mode)
        authority = root / "authority"
        authority.mkdir(parents=True, mode=0o700)
        # The historical PID slot deliberately names a live, unrelated owner.
        # It must neither block the actual kernel lock nor be changed on exit.
        evidence = authority / "pid"
        evidence.write_text(str(os.getpid()) + "\n")
        original_evidence = (evidence.stat().st_ino, evidence.read_bytes())
        private = authority / f".mcp-agent-mail-install.lock.{os.geteuid()}.d"
        private_evidence = None
        unsafe_before = None
        if mode == "precreated":
            private.mkdir(mode=0o700)
            private_evidence = private / "pid"
            private_evidence.write_text(str(os.getpid()) + "\n")
            private_before = (private_evidence.stat().st_ino, private_evidence.read_bytes())
        elif mode.startswith("precreated-"):
            victim = root / "private-victim"
            victim.mkdir(mode=0o700)
            witness = victim / "evidence"
            witness.write_bytes(b"private-authority victim must survive\n")
            witness_before = (witness.stat().st_ino, witness.read_bytes())
            if mode == "precreated-public":
                private.mkdir(mode=0o755)
                private.chmod(0o755)
            elif mode == "precreated-symlink":
                private.symlink_to(victim, target_is_directory=True)
            elif mode == "precreated-file":
                private.write_bytes(b"not a lock directory\n")
            else:
                os.mkfifo(private, 0o600)
            st = private.lstat()
            unsafe_before = (st.st_ino, st.st_mode, st.st_uid)
        log_path = Path(artifacts) / ("kernel-" + backend + "-" + mode + ".log")
        child_pid = None
        with log_path.open("wb") as log:
            owner = subprocess.Popen([bash, probe, str(root), backend, mode, library],
                                     stdin=subprocess.PIPE, stdout=log, stderr=subprocess.STDOUT)
        try:
            if unsafe_before is not None:
                owner.stdin.close()
                assert owner.wait(timeout=15) == 1, log_path.read_text()
                assert not (root / "ready").exists(), "unsafe authority reached acquired state"
                assert "Installer lock must be an owner-only non-symlink directory" in log_path.read_text()
                st = private.lstat()
                assert (st.st_ino, st.st_mode, st.st_uid) == unsafe_before
                assert (witness.stat().st_ino, witness.read_bytes()) == witness_before
                if mode == "precreated-file":
                    assert private.read_bytes() == b"not a lock directory\n"
                if mode == "precreated-symlink":
                    assert private.readlink() == victim
                assert (evidence.stat().st_ino, evidence.read_bytes()) == original_evidence
                assert not (root / "kernel.adapters").exists(), "unsafe authority reached a kernel helper"
                completed += 1
                print("kernel", backend, mode, "refused without changing precreated evidence")
                continue
            deadline = time.monotonic() + 15
            while not (root / "ready").exists() and owner.poll() is None and time.monotonic() < deadline:
                time.sleep(0.02)
            assert (root / "ready").exists(), log_path.read_text()
            assert (root / "kernel.adapters").read_text().splitlines() == adapters
            assert locked(authority), "helper exited without leaving its kernel lock with Bash"
            if mode == "ordinary":
                # Another installer must try every permitted adapter and still
                # fail against the same held authority, without acquiring state.
                contender_root = root / "contender"
                contender_root.mkdir()
                contender = subprocess.run([bash, probe, str(contender_root), backend,
                                            "denied", library, str(authority)],
                                           stdout=subprocess.PIPE, stderr=subprocess.STDOUT, timeout=15)
                assert contender.returncode == 0, contender.stdout.decode()
                assert (contender_root / "refused").read_text() == "refused\n"
                assert (contender_root / "kernel.adapters").read_text().splitlines() == adapters
                assert locked(authority), "a failed contender disturbed the owner's lock"
            if (root / "child").exists():
                child_pid = int((root / "child").read_text())
            retained = authority
            retained_private = private
            replacement_evidence = None
            replacement_slot = None
            if mode in ("rename", "symlink"):
                retained = root / "retained-authority"
                authority.rename(retained)
                retained_private = retained / private.name
                if mode == "symlink":
                    victim = root / "victim"
                    victim.mkdir(mode=0o700)
                    authority.symlink_to(victim, target_is_directory=True)
                else:
                    authority.mkdir(mode=0o700)
                (authority / "pid").write_text("replacement evidence must survive\n")
                replacement_slot = authority / "pid"
                replacement_evidence = ((authority / "pid").stat().st_ino,
                                        (authority / "pid").read_bytes())
                assert locked(retained), "rename lost the held inode lock"
                owner.stdin.write(b"verify\n")
                owner.stdin.flush()
            elif mode in ("authority-rename", "authority-symlink", "authority-permissions"):
                if mode == "authority-permissions":
                    private.chmod(0o755)
                else:
                    retained_private = root / "retained-private"
                    private.rename(retained_private)
                    if mode == "authority-symlink":
                        victim = root / "replacement-private-victim"
                        victim.mkdir(mode=0o700)
                        private.symlink_to(victim, target_is_directory=True)
                    else:
                        private.mkdir(mode=0o700)
                    replacement_slot = private / "pid"
                    replacement_slot.write_bytes(b"replacement private evidence must survive\n")
                    replacement_evidence = (replacement_slot.stat().st_ino, replacement_slot.read_bytes())
                assert locked(retained_private, primary=False), "private replacement lost the held inode lock"
                owner.stdin.write(b"verify\n")
                owner.stdin.flush()
            elif mode.startswith("descriptor-"):
                substitute = root / "substitute"
                if mode == "descriptor-directory":
                    substitute.mkdir(mode=0o700)
                elif mode == "descriptor-file":
                    substitute.write_bytes(b"not an installer lock\n")
                owner.stdin.write(b"verify\n")
                owner.stdin.flush()
            elif mode.endswith("crash"):
                owner.kill()
            else:
                owner.stdin.write(b"release\n")
                owner.stdin.flush()
            rc = owner.wait(timeout=15)
            assert rc == (-signal.SIGKILL if mode.endswith("crash") else 0), log_path.read_text()
            expected_adapters = adapters * (1 if mode.endswith("crash") else 2)
            assert (root / "kernel.adapters").read_text().splitlines() == expected_adapters
            if mode == "descendant-crash":
                assert locked(retained_private, primary=False), "a surviving installer child must still exclude a writer"
                os.kill(child_pid, signal.SIGTERM)
                child_pid = None
                deadline = time.monotonic() + 5
                while locked(retained_private, primary=False) and time.monotonic() < deadline:
                    time.sleep(0.02)
            assert not locked(retained_private, primary=False), "the old acquisition was not released"
            assert ((retained / "pid").stat().st_ino, (retained / "pid").read_bytes()) == original_evidence
            if private_evidence is not None:
                assert (private_evidence.stat().st_ino, private_evidence.read_bytes()) == private_before
            if replacement_evidence is not None:
                assert (root / "refused").read_text() == "refused\n"
                assert (replacement_slot.stat().st_ino, replacement_slot.read_bytes()) == replacement_evidence
                assert authority.is_symlink() == (mode == "symlink")
                if mode == "authority-symlink":
                    assert private.is_symlink() and private.readlink() == victim
            if mode == "authority-permissions":
                assert (root / "refused").read_text() == "refused\n"
                assert private.stat().st_mode & 0o777 == 0o755
            if mode.startswith("descriptor-"):
                assert (root / "refused").read_text() == "refused\n"
                if mode == "descriptor-file":
                    assert substitute.read_bytes() == b"not an installer lock\n"
            if mode == "descendant-normal":
                os.kill(child_pid, 0)  # explicit unlock works while the child remains alive
            completed += 1
            print("kernel", backend, mode, "exclusion, authority, and release verified")
        finally:
            if owner.poll() is None:
                owner.kill()
                owner.wait(timeout=5)
            owner.stdin.close()
            if child_pid is not None:
                try:
                    os.kill(child_pid, signal.SIGTERM)
                except ProcessLookupError:
                    pass
for backend in ("missing", "failing", "closed"):
    root = Path(scratch) / backend
    authority = root / "authority"
    authority.mkdir(parents=True, mode=0o700)
    log_path = Path(artifacts) / ("kernel-" + backend + ".log")
    with log_path.open("wb") as log:
        result = subprocess.run([bash, probe, str(root), backend, "fault", library],
                                stdout=log, stderr=subprocess.STDOUT, timeout=15)
    assert result.returncode == 0, log_path.read_text()
    assert (root / "refused").read_text() == "refused\n"
    calls = (root / "kernel.adapters").read_text().splitlines() if (root / "kernel.adapters").exists() else []
    assert calls == (["flock", "python3", "perl"] * 2 if backend == "failing" else []), calls
    assert not locked(authority, primary=False), "failure left the auxiliary lock held"
    if backend != "closed":
        assert not locked(authority), "failure left the destination lock held"
    completed += 1
    print("kernel", backend, "refused without acquired state or parent fd 3 changes")
assert completed, "no real kernel-lock adapter ran"
print(f"Kernel lock cases: {completed} passed; unavailable adapters: {','.join(unavailable) or 'none'}")
Path(artifacts, "kernel-count").write_text(str(completed))
PY
then
    kernel_cases=$(cat "$ARTIFACT_DIR/kernel-count")
else
    failures=$((failures + 1))
fi

# Bounded probes must select a usable runner before starting their command,
# retain exact output evidence, and never retry a command with side effects.
capture_probe="$scratch/capture-probe.sh"
cat > "$capture_probe" <<'PROBE'
#!/usr/bin/env bash
set -euo pipefail
root="$1" backend="$2" mode="$3"
source "$4"
TMP="$root"
real_python=$(type -P python3) || real_python=''
real_perl=$(type -P perl) || real_perl=''
command() {
    if [ "${FUNCNAME[1]:-}" = capture_command_with_timeout ] && [ "${1:-}" = -v ]; then
        case "$backend:${2:-}" in
            python:python3|perl:perl|timeout:timeout|gtimeout:gtimeout|\
            broken-python:python3|broken-python:perl|noop-python:python3|noop-python:perl|\
            broken:python3|broken:perl|noop:python3|noop:perl|invalid:python3|invalid:perl|\
            python-selected:*|perl-selected:perl|perl-selected:timeout|perl-selected:gtimeout) return 0 ;;
            *) return 1 ;;
        esac
    fi
    builtin command "$@"
}
capture_helper() {
    local helper="$1" phase=probe executable='' rc=0
    shift
    [ "${1:-}" != - ] || phase=run
    printf '%s:%s\n' "$helper" "$phase" >> "$root/runners"
    # Capability checks must not hand installation leases to helpers either.
    if ( : <&7 ) 2>/dev/null || ( : <&8 ) 2>/dev/null; then return 96; fi
    case "$backend:$helper" in
        broken:*|broken-python:python3) return 71 ;;
        noop:*|noop-python:python3) return 0 ;;
        invalid:*) printf 'AM_INSTALLER_BOUNDED_RUNNER\nextra\n'; return 0 ;;
    esac
    if [ "$phase" = run ] && [ "$mode" = empty-status ]; then
        return 0
    fi
    case "$helper" in
        python3) executable="$real_python" ;; perl) executable="$real_perl" ;;
    esac
    [ -n "$executable" ] || return 127
    "$executable" "$@" || rc=$?
    if [ "$phase" = run ]; then
        case "$mode" in
            malformed-status) printf 'untrustworthy\n' > "$4"; return 0 ;;
            nul-status) printf '0\0' > "$4"; return 0 ;;
            newline-status) printf '0\n' > "$4"; return 0 ;;
            out-of-range-status) printf '256' > "$4"; return 0 ;;
            inconsistent-status) printf '37' > "$4"; return 0 ;;
            removed-status) mv "$4" "$root/removed-status-witness"; return 0 ;;
            runner-failure) return 71 ;;
        esac
    fi
    return "$rc"
}
python3() { capture_helper python3 "$@"; }
perl() { capture_helper perl "$@"; }
printf 'parent descriptor\n' > "$root/fd3-sentinel"
exec 3< "$root/fd3-sentinel"
exec 7< "$root"; exec 8< "$root"
deadline=5
[ "$mode" != timeout ] || deadline=2
rc=0
capture_command_with_timeout "$deadline" "$real_python" "$root/target.py" "$root" "$mode" 'spaces ; $HOME * "quoted"' || rc=$?
IFS= read -r witness <&3
[ "$witness" = 'parent descriptor' ] || exit 95
: <&7; : <&8
printf '%s' "$CAPTURED_CMD_OUTPUT_EXACT" > "$root/exact.bin"
printf '%s' "$CAPTURED_CMD_OUTPUT" > "$root/display.bin"
printf '%s %s %s\n' "$rc" "$CAPTURED_CMD_STATUS" "$CAPTURED_CMD_OUTPUT_LOSSLESS" > "$root/status"
PROBE

capture_cases=0
if python3 - "$capture_probe" "$unit_library" "$scratch/capture" "$ARTIFACT_DIR" "$BASH" <<'PY'
import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import subprocess
import sys
import time

probe, library, scratch, artifacts, bash = sys.argv[1:]
completed = 0
unavailable = []
target = r'''
import json
import os
from pathlib import Path
import signal
import sys
import time

root, mode = Path(sys.argv[1]), sys.argv[2]
inherited = []
for fd in (7, 8):
    try:
        os.fstat(fd)
    except OSError:
        pass
    else:
        inherited.append(fd)
with (root / "executions.jsonl").open("a") as output:
    output.write(json.dumps({"args": sys.argv[3:], "inherited": inherited}) + "\n")
payload = (root / "payload.bin").read_bytes()
os.write(1, payload[:3])
os.write(2, payload[3:])
if mode == "nonzero":
    raise SystemExit(37)
if mode == "signal":
    os.kill(os.getpid(), signal.SIGTERM)
if mode == "timeout":
    (root / "parent.pid").write_text(str(os.getpid()))
    child = os.fork()
    if child == 0:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
    else:
        (root / "child.pid").write_text(str(child))
    time.sleep(30)
'''

def running(pid):
    result = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, timeout=3)
    state = result.stdout.strip()
    return bool(state) and not state.startswith(b"Z")

backends = (("python", "python3", ["python3:probe", "python3:run"]),
            ("perl", "perl", ["perl:probe", "perl:run"]),
            ("broken-python", "perl", ["python3:probe", "perl:probe", "perl:run"]),
            ("noop-python", "perl", ["python3:probe", "perl:probe", "perl:run"]),
            ("timeout", "timeout", ["timeout:run"]),
            ("gtimeout", "gtimeout", ["gtimeout:run"]))
cases = []
for backend, executable, trace in backends:
    if shutil.which(executable) is None:
        unavailable.append(backend)
        continue
    for mode in ("bytes", "extra-newline", "nul", "nonzero", "signal", "timeout"):
        status = 37 if mode == "nonzero" else 143 if mode == "signal" else (
            137 if backend in ("timeout", "gtimeout") else 124) if mode == "timeout" else 0
        cases.append((backend, mode, trace, status, 1))
for backend in ("missing", "broken", "noop", "invalid"):
    trace = [] if backend == "missing" else ["python3:probe", "perl:probe"]
    cases.append((backend, "bytes", trace, 125, 0))
for provider in ("python3", "perl"):
    if shutil.which(provider) is None:
        continue
    for mode in ("nonzero", "signal", "empty-status", "malformed-status", "nul-status", "newline-status",
                 "out-of-range-status", "inconsistent-status", "removed-status", "runner-failure"):
        status = 37 if mode == "nonzero" else 143 if mode == "signal" else 125
        backend = "python-selected" if provider == "python3" else "perl-selected"
        cases.append((backend, mode, [provider + ":probe", provider + ":run"],
                      status, 0 if mode == "empty-status" else 1))
for delivery in ("file", "stdin"):
    for backend, mode, trace, expected_status, expected_count in cases:
        root = Path(scratch) / (backend + "-" + mode + "-" + delivery)
        root.mkdir(parents=True)
        (root / "target.py").write_text(target)
        tools_dir = root / "bin"
        tools_dir.mkdir()
        for helper in ("timeout", "gtimeout"):
            executable = shutil.which(helper)
            if executable:
                # An external exec avoids the shell-function signal diagnostic
                # contaminating output that production captures from GNU timeout.
                wrapper = tools_dir / helper
                wrapper.write_text("#!" + bash + "\n" +
                    "printf '%s:run\\n' " + helper + " >> \"$CAPTURE_TEST_ROOT/runners\"\n" +
                    "if ( : <&7 ) 2>/dev/null || ( : <&8 ) 2>/dev/null; then exit 96; fi\n" +
                    "exec " + shlex.quote(executable) + ' "$@"\n')
                wrapper.chmod(0o700)
        payload = b"\t'quoted' \\ $() \xff\r\n" if mode == "bytes" else (
            b"am 0.3.40\n\n" if mode == "extra-newline" else b"before\0after\n" if mode == "nul" else b"probe\n")
        (root / "payload.bin").write_bytes(payload)
        log_path = Path(artifacts) / ("capture-" + root.name + ".log")
        args = [bash] + ([probe] if delivery == "file" else ["-s", "--"])
        args += [str(root), backend, mode, library]
        try:
            with log_path.open("wb") as log:
                result = subprocess.run(args, input=Path(probe).read_bytes() if delivery == "stdin" else b"",
                                        stdout=log, stderr=subprocess.STDOUT, timeout=15,
                                        env=dict(os.environ, LC_ALL="C", CAPTURE_TEST_ROOT=str(root),
                                                 PATH=str(tools_dir) + os.pathsep + os.environ["PATH"]))
            assert result.returncode == 0, log_path.read_text(errors="replace")
            rc, status, lossless = map(int, (root / "status").read_text().split())
            assert (rc, status) == (expected_status, expected_status), (backend, mode, rc, status)
            actual_trace = (root / "runners").read_text().splitlines() if (root / "runners").exists() else []
            assert actual_trace == trace, (backend, mode, actual_trace)
            records = [json.loads(line) for line in (root / "executions.jsonl").read_text().splitlines()] if (root / "executions.jsonl").exists() else []
            assert len(records) == expected_count, "a failed runner retried or unexpectedly started the target"
            for record in records:
                assert record == {"args": ['spaces ; $HOME * "quoted"'], "inherited": []}, record
            if mode == "removed-status":
                assert (root / "removed-status-witness").read_bytes() == b"0"
            exact = (root / "exact.bin").read_bytes()
            if mode == "nul":
                assert lossless == 0 and exact != payload, "NUL loss was not detected"
            else:
                assert lossless == 1
                if expected_count:
                    assert exact == payload, (backend, mode, exact)
                    assert (root / "display.bin").read_bytes() == payload.rstrip(b"\n")
            if mode == "timeout":
                for name in ("parent.pid", "child.pid"):
                    pid = int((root / name).read_text())
                    deadline = time.monotonic() + 2
                    while running(pid) and time.monotonic() < deadline:
                        time.sleep(0.02)
                    assert not running(pid), "timeout left a process-group member running"
            completed += 1
            print("capture", backend, mode, delivery, "status, bytes, execution count, and descriptors verified")
        finally:
            for name in ("parent.pid", "child.pid"):
                if (root / name).exists():
                    try:
                        os.kill(int((root / name).read_text()), signal.SIGKILL)
                    except ProcessLookupError:
                        pass
print(f"Capture cases: {completed} passed; unavailable runners: {','.join(unavailable) or 'none'}")
Path(artifacts, "capture-count").write_text(str(completed))
PY
then
    capture_cases=$(cat "$ARTIFACT_DIR/capture-count")
else
    failures=$((failures + 1))
fi

# The external Claude CLI cannot inherit write authority from a missing or
# failed metadata check. Exercise the real guard and caller after the native
# credential phase, with no other client candidates and a harmless CLI witness.
config_guard_probe="$scratch/config-guard-probe.sh"
cat > "$config_guard_probe" <<'PROBE'
#!/usr/bin/env bash
set -euo pipefail
root="$1" backend="$2" mode="$3" target="$4"
source "$5"
QUIET=0 VERBOSE=0 HAS_GUM=0 NO_GUM=1 LOG_INITIALIZED=0
LOG_FILE="$root/verbose.log" DRY_RUN=0
HTTP_BEARER_TOKEN=guard-fixture-token
export HTTP_BEARER_TOKEN
real_python=$(type -P python3) || real_python=''
real_perl=$(type -P perl) || real_perl=''
command() {
    if [ "${FUNCNAME[1]:-}" = config_target_is_hardlink_aliased ] && [ "${1:-}" = -v ]; then
        case "$backend:${2:-}" in
            python:python3|perl:perl|broken-python:*|failing:*|noop:*|malformed:*|nonzero-safe:*|unsafe-first:*) return 0 ;;
            *) return 1 ;;
        esac
    fi
    builtin command "$@"
}
metadata_helper() {
    local helper="$1" executable=''
    shift
    if [ "${FUNCNAME[2]:-}" = config_target_is_hardlink_aliased ]; then
        printf '%s:%s\n' "$stage" "$helper" >> "$root/providers"
        if ( : <&7 ) 2>/dev/null || ( : <&8 ) 2>/dev/null; then
            printf 'leaked\n' > "$root/helper-fd-leak"
            return 96
        fi
        case "$backend:$helper" in
            failing:*|broken-python:python3) return 71 ;;
            noop:*) return 0 ;;
            malformed:*) printf 'safe\nextra\n'; return 0 ;;
            nonzero-safe:*) printf 'safe\n'; return 71 ;;
            unsafe-first:perl) printf 'safe\n'; return 0 ;;
        esac
    fi
    case "$helper" in python3) executable="$real_python" ;; perl) executable="$real_perl" ;; esac
    [ -n "$executable" ] || return 127
    "$executable" "$@"
}
python3() { metadata_helper python3 "$@"; }
perl() { metadata_helper perl "$@"; }
# Other client kinds and service discovery are outside this admission seam.
detect_mcp_configs() { return 0; }
desired_mcp_http_url() { printf 'http://127.0.0.1:8765/mcp/'; }
printf 'before\nafter\n' > "$root/fd3-sentinel"
exec 3< "$root/fd3-sentinel"
exec 7< "$root"; exec 8< "$root"
cd "$root/project"
stage=direct
rc=0
config_target_is_hardlink_aliased "$target" || rc=$?
printf '%s\n' "$rc" > "$root/guard-status"
IFS= read -r witness <&3
[ "$witness" = before ] || exit 95
if [ "$mode" != device ]; then
    containment=0
    path_resolves_within_directory "$target" "$PWD" || containment=$?
    printf '%s\n' "$containment" > "$root/containment-status"
    stage=caller
    setup_mcp_configs /unused/mcp-agent-mail
fi
IFS= read -r witness <&3
[ "$witness" = after ] || exit 95
: <&7; : <&8
PROBE

config_guard_cases=0
if python3 - "$config_guard_probe" "$unit_library" "$scratch/config-guard" "$ARTIFACT_DIR" "$BASH" <<'PY'
import errno
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys

probe, library, scratch, artifacts, bash = sys.argv[1:]
completed = 0
unavailable = []
cases = []
for backend, executable, providers in (("python", "python3", ["python3"]),
        ("perl", "perl", ["perl"]), ("broken-python", "perl", ["python3", "perl"])):
    if shutil.which(executable) is None:
        unavailable.append(backend)
        continue
    for mode in ("regular", "absent", "hardlink", "symlink", "dangling", "loop", "directory", "fifo",
                 "missing-parent", "parent-file", "symlink-parent", "dangling-parent", "loop-parent", "device"):
        cases.append((backend, mode, providers, mode in ("regular", "absent")))
for backend, mode in (("missing", "regular"), ("failing", "hardlink"), ("noop", "regular"),
                      ("malformed", "regular"), ("nonzero-safe", "hardlink"), ("unsafe-first", "hardlink")):
    providers = [] if backend == "missing" else ["python3"] if backend == "unsafe-first" else ["python3", "perl"]
    cases.append((backend, mode, providers, False))

def snapshot(path):
    try:
        metadata = path.lstat()
    except OSError as error:
        assert error.errno in (errno.ENOENT, errno.ENOTDIR, errno.ELOOP), error
        return ("error", error.errno)
    content = path.read_bytes() if stat.S_ISREG(metadata.st_mode) else (
        os.readlink(path) if stat.S_ISLNK(metadata.st_mode) else None)
    return (metadata.st_dev, metadata.st_ino, metadata.st_mode, metadata.st_nlink, content)

for delivery in ("file", "stdin"):
    for backend, mode, providers, admitted in cases:
        root = Path(scratch) / (backend + "-" + mode + "-" + delivery)
        for directory in (root / "project", root / "bin"):
            directory.mkdir(parents=True)
        home = root / "home"
        if mode == "parent-file":
            home.write_bytes(b"parent is a regular file\n")
        elif mode == "symlink-parent":
            (root / "real-home").mkdir()
            home.symlink_to(root / "real-home", target_is_directory=True)
        elif mode in ("dangling-parent", "loop-parent"):
            home.symlink_to(home if mode == "loop-parent" else root / "missing-home")
        elif mode != "missing-parent":
            home.mkdir()
        target = Path("/dev/null") if mode == "device" else home / ".claude.json"
        victim = root / ("outside.json" if mode == "symlink" else "project/protected.json")
        victim.write_bytes(b'{"preserve":"original bytes"}\n')
        if mode == "regular":
            target.write_bytes(b'{"preserve":"client bytes"}\n')
        elif mode == "hardlink":
            os.link(victim, target)
        elif mode in ("symlink", "dangling", "loop"):
            target.symlink_to(victim if mode == "symlink" else target if mode == "loop" else root / "absent-victim")
        elif mode == "directory":
            target.mkdir()
        elif mode == "fifo":
            os.mkfifo(target, 0o600)
        before = (snapshot(target), snapshot(home), snapshot(victim))
        cli = root / "bin/claude"
        cli.write_text("#!" + bash + r'''
if ( : <&7 ) 2>/dev/null || ( : <&8 ) 2>/dev/null; then
    printf 'leaked\n' > "$CLAUDE_GUARD_ROOT/cli-fd-leak"
    exit 96
fi
[ "${1:-}" = mcp ] || exit 97
printf '%s\n' "${2:-}" >> "$CLAUDE_GUARD_ROOT/cli-calls"
if [ "${2:-}" = add ]; then
    printf 'admitted\n' > "$CLAUDE_GUARD_ROOT/cli-write"
fi
exit 0
''')
        cli.chmod(0o700)
        log_path = Path(artifacts) / ("config-guard-" + root.name + ".log")
        args = [bash] + ([probe] if delivery == "file" else ["-s", "--"])
        args += [str(root), backend, mode, str(target), library]
        with log_path.open("wb") as log:
            result = subprocess.run(args, input=Path(probe).read_bytes() if delivery == "stdin" else b"",
                                    stdout=log, stderr=subprocess.STDOUT, timeout=15,
                                    env=dict(os.environ, HOME=str(home), CLAUDE_GUARD_ROOT=str(root),
                                             PATH=str(root / "bin") + os.pathsep + os.environ["PATH"]))
        assert result.returncode == 0, log_path.read_text(errors="replace")
        assert int((root / "guard-status").read_text()) == int(admitted), (backend, mode)
        assert not (root / "helper-fd-leak").exists() and not (root / "cli-fd-leak").exists()
        expected_providers = ["direct:" + provider for provider in providers]
        if mode != "device" and (root / "containment-status").read_text().strip() == "1":
            expected_providers += ["caller:" + provider for provider in providers]
        actual_providers = (root / "providers").read_text().splitlines() if (root / "providers").exists() else []
        assert actual_providers == expected_providers, (backend, mode, actual_providers)
        calls = (root / "cli-calls").read_text().splitlines() if (root / "cli-calls").exists() else []
        assert calls == (["remove", "add"] if admitted else []), (backend, mode, calls)
        assert (root / "cli-write").exists() == admitted
        assert (snapshot(target), snapshot(home), snapshot(victim)) == before, "admission changed protected bytes or object identity"
        completed += 1
        print("config-guard", backend, mode, delivery, "metadata, writer admission, and descriptor lifetime verified")
print(f"Config guard cases: {completed} passed; unavailable providers: {','.join(unavailable) or 'none'}")
Path(artifacts, "config-guard-count").write_text(str(completed))
PY
then
    config_guard_cases=$(cat "$ARTIFACT_DIR/config-guard-count")
else
    failures=$((failures + 1))
fi

# Native commands can start processes that outlive the installer. Exercise the
# real launch functions with harmless executables that inspect their inherited
# descriptors and independently test the parent's two kernel locks. Service
# discovery/readiness are explicit fixtures; no real service is installed.
native_probe="$scratch/native-lock-probe.sh"
cat > "$native_probe" <<'PROBE'
#!/usr/bin/env bash
set -Eeuo pipefail
root="$1" mode="$2"
source "$3"
QUIET=0 VERBOSE=0 HAS_GUM=0 NO_GUM=1 DRY_RUN=0
LOG_INITIALIZED=0 ERROR_TAIL_EMITTED=0 VERBOSE_DUMP_LINES=20
LOG_FILE="$root/verbose.log"
ISSUES_URL=https://example.invalid/installer-native-lock-test
INSTALLER_EXIT_SUCCESS=0
BINARY_TRANSACTION_ACTIVE_INSTALL_DIR=''
BINARY_TRANSACTION_RECOVERY_ACTIVE=0
BINARY_TRANSACTION_EXIT_RECOVERY_ATTEMPTED=0
LOCKED=0 DESTINATION_LOCKED=0
DEST="$root/destination" BIN_CLI=am
TMP="$root/mcp-agent-mail-install.native"
mkdir "$TMP"
printf 'temporary install data\n' > "$TMP/payload"
trap cleanup EXIT
acquire_installer_lock "$DEST" destination
acquire_installer_lock "$root/additional" initial
case "$mode" in
    query) installed_am_db_query_scalar "$root/fixture.db" 'SELECT 1' ;;
    exec) installed_am_db_exec "$root/fixture.db" 'SELECT 1' ;;
    exec-stdin) printf 'SELECT 1;\n' | installed_am_db_exec "$root/fixture.db" ;;
    backup) installed_am_db_backup "$root/fixture.db" "$root/fixture.backup" ;;
    bounded|bounded-without-python|bounded-timeout)
        command() {
            if [ "${FUNCNAME[1]:-}" = capture_command_with_timeout ] && [ "${1:-}" = -v ]; then
                case "$mode:${2:-}" in
                    bounded:python3|bounded-without-python:perl|bounded-timeout:timeout|bounded-timeout:gtimeout) ;;
                    *) return 1 ;;
                esac
            fi
            builtin command "$@"
        }
        rc=0
        capture_command_with_timeout 5 "$DEST/am" --probe || rc=$?
        unset -f command
        [ "$rc" -eq 0 ] || exit "$rc"
        [ "$CAPTURED_CMD_OUTPUT" = probe-ok ] || exit 95
        ;;
    service|service-unready)
        # These fixtures admit the isolated CLI executable, never host service
        # configuration or a network endpoint. The production launch remains.
        has_remote_http_client_targets() { return 0; }
        desired_mcp_http_url() { printf 'http://127.0.0.1:1/mcp/'; }
        probe_remote_http_endpoint() { return 1; }
        service_management_allowed() { return 0; }
        platform_supports_user_service_management() { return 0; }
        desired_service_bind_host() { printf '127.0.0.1'; }
        desired_service_bind_port() { printf '1'; }
        repair_launchd_service_env_from_rust_config() { return 0; }
        service_setup_unavailable_failure() { return 1; }
        wait_for_remote_http_endpoint() {
            [ "$mode" = service ] && [ -f "$root/service-started" ]
        }
        REMOTE_HTTP_PROBE_DETAIL=fixture
        if ensure_remote_http_client_readiness; then
            [ "$mode" = service ] || exit 96
        else
            [ "$mode" = service-unready ] || exit 96
        fi
        ;;
    interactive|noninteractive)
        MAC_DIRECT_EXEC_COMPAT_MODE=1
        SHELL="$root/bin/bash"
        activate_mac_python_cli_compat_shell
        if [ "$mode" = interactive ]; then exit 97; fi
        printf 'noninteractive\n' > "$root/no-exec"
        verify_installer_lock_authorities
        ;;
    *) exit 98 ;;
esac
INSTALLER_EXIT_SUCCESS=1
PROBE

native_cases=0
if python3 - "$native_probe" "$unit_library" "$scratch/native" "$ARTIFACT_DIR" "$BASH" <<'PY'
import fcntl
import json
import os
from pathlib import Path
import pty
import shutil
import subprocess
import sys
import time

probe, library, scratch, artifacts, bash = sys.argv[1:]
base = Path(scratch)
base.mkdir()
completed = 0
unavailable = []

executable = r'''#!/usr/bin/env python3
import fcntl
import json
import os
from pathlib import Path
import sys

root = Path(os.environ["PROBE_ROOT"])
interactive = Path(sys.argv[0]).name == "bash"
inherited = []
for fd in (7, 8):
    try:
        os.fstat(fd)
    except OSError:
        pass
    else:
        inherited.append(fd)
held = []
for directory in (root / "destination" / f".mcp-agent-mail-install.lock.{os.geteuid()}.d", root / "additional"):
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            held.append(True)
        else:
            held.append(False)
    finally:
        os.close(fd)
record = {"args": sys.argv[1:], "inherited": inherited, "held": held,
          "interactive": interactive}
with (root / "executions.jsonl").open("a") as output:
    output.write(json.dumps(record) + "\n")
if inherited or held != ([False, False] if interactive else [True, True]):
    raise SystemExit(94)
if interactive:
    (root / "shell-ready").write_text("ready\n")
    sys.stdin.readline()
elif sys.argv[1:] == ["--probe"]:
    print("probe-ok")
elif sys.argv[1:3] == ["service", "install"] and "--help" not in sys.argv[1:]:
    (root / "service-started").write_text("isolated fixture only\n")
elif sys.argv[1:3] == ["service", "status"]:
    print("isolated service is deliberately not ready")
    raise SystemExit(1)
elif sys.argv[1:3] == ["tooling", "db-query"]:
    print("1")
'''

def locked(directory):
    if directory.name == "destination":
        directory = directory / f".mcp-agent-mail-install.lock.{os.geteuid()}.d"
    fd = os.open(directory, os.O_RDONLY | os.O_DIRECTORY)
    try:
        try:
            fcntl.flock(fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            return True
        return False
    finally:
        os.close(fd)

for mode in ("query", "exec", "exec-stdin", "backup", "bounded",
             "bounded-without-python", "bounded-timeout", "service", "service-unready",
             "noninteractive", "interactive"):
    if (mode == "bounded-without-python" and not shutil.which("perl")) or (
            mode == "bounded-timeout" and not (shutil.which("timeout") or shutil.which("gtimeout"))):
        unavailable.append(mode)
        continue
    root = base / mode
    for directory in ("home", "destination", "additional", "bin"):
        (root / directory).mkdir(parents=True, mode=0o700)
    for target in (root / "destination/am", root / "bin/bash"):
        target.write_text(executable)
        target.chmod(0o700)
    env = {"HOME": str(root / "home"), "PATH": os.environ["PATH"],
           "PROBE_ROOT": str(root), "LC_ALL": "C", "TERM": "dumb"}
    master = slave = None
    if mode == "interactive":
        master, slave = pty.openpty()
    log_path = Path(artifacts) / ("native-lock-" + mode + ".log")
    with log_path.open("wb") as log:
        process = subprocess.Popen([bash, probe, str(root), mode, library], env=env,
                                   stdin=slave if slave is not None else subprocess.DEVNULL,
                                   stdout=log, stderr=subprocess.STDOUT)
    if slave is not None:
        os.close(slave)
    try:
        if mode == "interactive":
            deadline = time.monotonic() + 15
            while process.poll() is None and not (root / "shell-ready").exists() and time.monotonic() < deadline:
                time.sleep(0.02)
            assert process.poll() is None and (root / "shell-ready").exists(), log_path.read_text()
            assert not locked(root / "destination"), "interactive exec retained the destination lease"
            assert not locked(root / "additional"), "interactive exec retained the additional lease"
            assert not (root / "mcp-agent-mail-install.native").exists(), "exec bypassed EXIT cleanup"
            os.write(master, b"exit\n")
        assert process.wait(timeout=15) == 0, log_path.read_text()
        assert not locked(root / "destination")
        assert not locked(root / "additional")
        assert not (root / "mcp-agent-mail-install.native").exists()
        records_path = root / "executions.jsonl"
        records = [json.loads(line) for line in records_path.read_text().splitlines()] if records_path.exists() else []
        expected_count = 0 if mode == "noninteractive" else 3 if mode == "service-unready" else 2 if mode == "service" else 1
        assert len(records) == expected_count, (mode, records, log_path.read_text())
        for record in records:
            assert not record["inherited"], (mode, record)
            assert record["held"] == ([False, False] if mode == "interactive" else [True, True]), (mode, record)
        if mode == "service-unready":
            assert records[-1]["args"] == ["service", "status"], records
        if mode == "noninteractive":
            assert (root / "no-exec").read_text() == "noninteractive\n"
        completed += 1
        print("native-lock", mode, "descriptor ownership and lifetime verified", flush=True)
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        if master is not None:
            os.close(master)
assert completed, "no native execution boundary ran"
print(f"Native lock cases: {completed} passed; unavailable runners: {','.join(unavailable) or 'none'}")
(base / "native-count").write_text(str(completed))
PY
then
    native_cases=$(cat "$scratch/native/native-count")
else
    failures=$((failures + 1))
fi

printf 'Installer boundary regressions: %s exit cases, %s lock cases, %s destination cases, %s identity cases, %s kernel cases, %s capture cases, %s config guard cases, %s native cases, %s failure(s)\n' "$unit_cases" "$lock_cases" "$destination_cases" "$identity_cases" "$kernel_cases" "$capture_cases" "$config_guard_cases" "$native_cases" "$failures"
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
install_test_shell=/bin/bash
if [ "$(uname -s)" = Darwin ]; then
    # Bash still runs the installer; SHELL selects the user's startup files.
    # The report's macOS sandbox updated .zshenv/.profile, not .bashrc.
    install_test_shell=/bin/zsh
fi

for mode in file stdin file-default stdin-default; do
    case_root="$scratch/$mode"
    mkdir -p "$case_root/home" "$case_root/work" "$case_root/tmp"
    trace="$ARTIFACT_DIR/$mode.trace.log"
    verbose_log="$ARTIFACT_DIR/$mode.verbose.log"
    rc=0
    (
        cd "$case_root/work" || exit
        # No caller credentials, XDG locations, shell startup hooks, or installer
        # skip overrides may leak into the clean-HOME reproduction.
        case "$mode" in
            file)
                env -i HOME="$case_root/home" PATH="$install_test_path" SHELL="$install_test_shell" \
                    TMPDIR="$case_root/tmp" LOG_FILE="$verbose_log" LC_ALL=C TERM=dumb \
                    "$BASH" -x "$INSTALL_SH" --version "$VERSION" --yes --no-gum --no-service --verbose < /dev/null
                ;;
            stdin)
                env -i HOME="$case_root/home" PATH="$install_test_path" SHELL="$install_test_shell" \
                    TMPDIR="$case_root/tmp" LOG_FILE="$verbose_log" LC_ALL=C TERM=dumb \
                    "$BASH" -x -s -- --version "$VERSION" --yes --no-gum --no-service --verbose < "$INSTALL_SH"
                ;;
            file-default)
                # Match GH#327's argument-less file invocation. Pin only the
                # release in the environment; do not change verbosity, service
                # policy, confirmations, or the empty-argument Bash 3.2 path.
                env -i HOME="$case_root/home" PATH="$install_test_path" SHELL="$install_test_shell" \
                    TMPDIR="$case_root/tmp" LOG_FILE="$verbose_log" LC_ALL=C TERM=dumb VERSION="$VERSION" \
                    "$BASH" "$INSTALL_SH" < /dev/null
                ;;
            stdin-default)
                env -i HOME="$case_root/home" PATH="$install_test_path" SHELL="$install_test_shell" \
                    TMPDIR="$case_root/tmp" LOG_FILE="$verbose_log" LC_ALL=C TERM=dumb VERSION="$VERSION" \
                    "$BASH" < "$INSTALL_SH"
                ;;
        esac
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
        'update_mcp_configs:output No coding agents detected.' \
        'install:complete rc=0'; do
        if ! grep -Fq "$witness" "$verbose_log"; then
            printf '%s: missing real-path witness: %s\n' "$mode" "$witness" >&2
            failures=$((failures + 1))
        fi
    done
    unexpected_error=0
    if grep -q '^++* on_error ' "$trace"; then
        unexpected_error=1
    fi
    case "$mode" in
        *-default)
            # These invocations have no xtrace, so check the diagnostic itself.
            # Do not search for this literal in traced runs: persisting the
            # piped installer logs its source, which contains the same text.
            if grep -Fq 'Unexpected installer error.' "$trace"; then
                unexpected_error=1
            fi
            ;;
    esac
    if [ "$unexpected_error" -ne 0 ]; then
        echo "$mode: ERR handler ran on the successful-install path" >&2
        failures=$((failures + 1))
    fi
done
printf 'Installer exit regression: %s failure(s); evidence: %s\n' "$failures" "$ARTIFACT_DIR"
[ "$failures" -eq 0 ]
