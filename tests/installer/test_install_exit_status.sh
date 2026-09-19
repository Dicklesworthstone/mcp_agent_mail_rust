#!/usr/bin/env bash
# Real signed-release regression for GH#327. Run with the host's stock Bash.
# This deliberately exercises the complete installer, including its EXIT trap;
# extracting functions or testing `am --version` alone cannot prove success.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"
VERSION="${AM_INSTALLER_EXIT_VERSION:-v0.3.36}"
ARTIFACT_DIR="${AM_INSTALLER_EXIT_ARTIFACT_DIR:-$REPO_ROOT/tests/artifacts/installer-exit}"
mkdir -p "$ARTIFACT_DIR"
ARTIFACT_DIR="$(cd "$ARTIFACT_DIR" && pwd -P)"
[ -f "$INSTALL_SH" ] || { echo "Missing installer: $INSTALL_SH" >&2; exit 2; }
command -v minisign >/dev/null 2>&1 || { echo 'This real-path test requires minisign.' >&2; exit 2; }

# Resolve macOS /var and /tmp aliases before giving paths to the installer.
# Its anti-symlink destination checks must remain enabled, not be worked around
# with --no-verify or a mocked path validator.
scratch="$(mktemp -d)"
scratch="$(cd "$scratch" && pwd -P)"
printf 'Bash: %s\nScratch: %s\nVersion: %s\n' "$BASH_VERSION" "$scratch" "$VERSION"
failures=0
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
            env -i HOME="$case_root/home" PATH="$PATH" SHELL=/bin/bash \
                TMPDIR="$case_root/tmp" LOG_FILE="$verbose_log" LC_ALL=C TERM=dumb \
                "$BASH" -x "$INSTALL_SH" --version "$VERSION" --yes --no-gum --no-service --verbose
        else
            env -i HOME="$case_root/home" PATH="$PATH" SHELL=/bin/bash \
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
