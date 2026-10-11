#!/usr/bin/env bash
# Regression for GH#342: the installer must not enable `lastpipe`.
#
# Bash (5.2.21 confirmed from the ACFS core, and later releases per the
# upstream report) records the last element of a lastpipe pipeline with
# append_process(), which leaves the job's process list half-linked while
# SIGCHLD is unblocked. A pipeline child that exits in that window crashes the
# shell with SIGSEGV. A fresh ACFS install hit it in the forked `$(grep | grep |
# head -1 || true)` child of detect_python_alias and the installer stopped with
# exit 139. append_process() is only reachable under lastpipe, so keeping the
# option off removes the crash; this test also guards that no pipeline relies
# on lastpipe, which would make turning it back on look necessary.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="${AM_INSTALLER_NO_LASTPIPE_SOURCE:-$REPO_ROOT/install.sh}"
[ -f "$INSTALL_SH" ] || { echo "Missing installer: $INSTALL_SH" >&2; exit 2; }
failures=0

# 1. No live `shopt -s ... lastpipe` (comments are allowed to mention it).
enabling=$(awk '
    { line = $0; sub(/^[[:space:]]+/, "", line) }
    line ~ /^#/ { next }
    line ~ /shopt[[:space:]]+(-[a-z]*s[a-z]*[[:space:]]+)([[:alnum:]_]+[[:space:]]+)*lastpipe/ { print FNR ": " $0 }
' "$INSTALL_SH")
if [ -n "$enabling" ]; then
    echo "FAIL: install.sh enables lastpipe (GH#342):" >&2
    printf '  %s\n' "$enabling" >&2
    failures=$((failures + 1))
else
    echo "PASS: install.sh does not enable lastpipe"
fi

# 2. No pipeline whose last element is a builtin or compound command, on one
#    line or continued from a line ending in `|`. Such a pipeline would only
#    keep its variables with lastpipe; use `while ...; done < <(cmd)` instead.
relying=$(awk '
    function code(s) { sub(/[[:space:]]#.*$/, "", s); return s }
    {
        raw = $0; trimmed = raw; sub(/^[[:space:]]+/, "", trimmed)
        if (trimmed ~ /^#/) { prev_pipe = 0; next }
        line = code(raw)
        if (prev_pipe && trimmed ~ /^(while|until|read|mapfile|readarray|\{)/) print FNR ": " raw
        probe = line; gsub(/\|\|/, "", probe)
        if (probe ~ /\|[[:space:]]*(while|until|read|mapfile|readarray|IFS=[^[:space:]]*[[:space:]]+read|\{)([[:space:];]|$)/) print FNR ": " raw
        prev_pipe = (probe ~ /\|[[:space:]]*\\?[[:space:]]*$/)
    }
' "$INSTALL_SH")
if [ -n "$relying" ]; then
    echo "FAIL: pipeline ends in a builtin/compound command that would need lastpipe:" >&2
    printf '  %s\n' "$relying" >&2
    failures=$((failures + 1))
else
    echo "PASS: no pipeline relies on lastpipe"
fi

# 3. Runtime: the installer's own prologue leaves lastpipe off.
prologue=$(awk '/^VERSION=/ { exit } { print }' "$INSTALL_SH")
state=$(bash -c "$prologue"$'\n''shopt -q lastpipe && echo on || echo off' 2>/dev/null || true)
if [ "$state" = "off" ]; then
    echo "PASS: lastpipe is off after the installer prologue"
else
    echo "FAIL: lastpipe after the installer prologue: ${state:-unknown}" >&2
    failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
    echo "$failures check(s) failed" >&2
    exit 1
fi
echo "All lastpipe regression checks passed"
