#!/usr/bin/env bash
#
# tests/installer/test_install_warn_git_251.sh — br-8ujfs.1.3 (A3)
#
# Verifies that install.sh emits the AM_INSTALL_WARN GIT_2_51_0_KNOWN_BAD
# marker when the detected git is 2.51.0, and does NOT emit it for safe
# versions.
#
# Approach: PATH-shim a fake git binary with controlled `--version`
# output, source install.sh just enough to import check_git_version_known_bad,
# invoke it, and assert on stderr output.
#
# We deliberately do NOT run the full install.sh (it tries to download a
# release asset, which we don't want to do in a test). Instead we source
# it so its functions are in scope, then call only the specific check.
# To make sourcing safe we short-circuit the auto-run at the bottom.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

if [ ! -f "$INSTALL_SH" ]; then
    echo "FATAL: $INSTALL_SH not found" >&2
    exit 2
fi

ts() { date -u +%Y-%m-%dT%H:%M:%SZ; }
step() { echo "[A3_TEST $(ts)] $*" >&2; }

artifact_dir="$REPO_ROOT/tests/artifacts/installer"
mkdir -p "$artifact_dir"
artifact_log="$artifact_dir/run-$(date -u +%Y%m%dT%H%M%SZ).log"

step "artifact log: $artifact_log"
exec 3>"$artifact_log"

trap 'exec 3>&-' EXIT

# -----------------------------------------------------------------------
# Build a shim git in a tempdir.
# -----------------------------------------------------------------------
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"; exec 3>&-' EXIT

build_shim() {
    local version="$1"
    local bin="$tmp/bin/git"
    mkdir -p "$tmp/bin"
    cat >"$bin" <<SHIM
#!/usr/bin/env bash
if [ "\$1" = "--version" ]; then
    echo "git version $version"
    exit 0
fi
# Forward anything else to /bin/true so install.sh preflight passes.
exit 0
SHIM
    chmod +x "$bin"
}

# Load install.sh's functions without running preflight_checks at bottom.
# The script's tail calls preflight_checks then drives install; we stop
# it by preemptively defining a no-op main. We source with a sentinel
# env var so install.sh can skip the auto-run.
#
# We actually cannot rely on a sentinel since install.sh doesn't
# currently check one. Workaround: extract just the function body via
# sed and source that.
step "extracting check_git_version_known_bad from install.sh"
extract="$tmp/check_fn.sh"
{
    # Globals referenced by the extracted helpers. install.sh sets these
    # at top of script; when we source just the functions we re-declare
    # them with conservative defaults that exercise the plain-echo paths.
    echo 'QUIET=0'
    echo 'HAS_GUM=0'
    echo 'NO_GUM=1'
    echo 'VERBOSE=0'
    # warn() is the one we actually need to assert on (emits the marker).
    sed -n '/^warn() {/,/^}/p' "$INSTALL_SH"
    # info() and verbose() are stubbed (install.sh's verbose() references
    # init_verbose_log which we don't want to pull into the test harness).
    echo 'info() { :; }'
    echo 'verbose() { :; }'
    sed -n '/^check_git_version_known_bad() {/,/^}/p' "$INSTALL_SH"
} >"$extract"

if ! grep -q 'check_git_version_known_bad' "$extract"; then
    echo "FATAL: could not extract check_git_version_known_bad from install.sh" >&2
    echo "(Does the function exist? Check install.sh change under A3.)" >&2
    exit 2
fi

# -----------------------------------------------------------------------
# Scenario A: shim says 2.51.0 → expect warning marker.
# -----------------------------------------------------------------------
step "scenario A: shim reports 2.51.0"
build_shim "2.51.0"
set +e
output_a=$(PATH="$tmp/bin:$PATH" bash -c "source '$extract'; check_git_version_known_bad" 2>&1)
rc_a=$?
set -e
echo "=== scenario A (git 2.51.0) stderr ===" >&3
echo "$output_a" >&3
echo "=== exit: $rc_a ===" >&3

if [ "$rc_a" -ne 0 ]; then
    echo "FAIL: check_git_version_known_bad exited nonzero ($rc_a) on 2.51.0 shim" >&2
    echo "$output_a" >&2
    exit 1
fi

if ! grep -q 'AM_INSTALL_WARN GIT_2_51_0_KNOWN_BAD' <<<"$output_a"; then
    echo "FAIL: 2.51.0 shim did NOT emit AM_INSTALL_WARN GIT_2_51_0_KNOWN_BAD marker" >&2
    echo "Captured stderr:" >&2
    echo "$output_a" >&2
    exit 1
fi
step "scenario A PASSED: marker emitted for 2.51.0"

# -----------------------------------------------------------------------
# Scenario B: shim says 2.50.2 → expect no marker.
# -----------------------------------------------------------------------
step "scenario B: shim reports 2.50.2"
build_shim "2.50.2"
set +e
output_b=$(PATH="$tmp/bin:$PATH" bash -c "source '$extract'; check_git_version_known_bad" 2>&1)
rc_b=$?
set -e
echo "=== scenario B (git 2.50.2) stderr ===" >&3
echo "$output_b" >&3
echo "=== exit: $rc_b ===" >&3

if [ "$rc_b" -ne 0 ]; then
    echo "FAIL: check_git_version_known_bad exited nonzero ($rc_b) on 2.50.2 shim" >&2
    exit 1
fi

if grep -q 'AM_INSTALL_WARN GIT_2_51_0_KNOWN_BAD' <<<"$output_b"; then
    echo "FAIL: 2.50.2 shim unexpectedly emitted the 2.51.0 marker" >&2
    echo "Captured stderr:" >&2
    echo "$output_b" >&2
    exit 1
fi
step "scenario B PASSED: no marker for 2.50.2"

# -----------------------------------------------------------------------
# Scenario C: shim says 2.52.0-rc1 → expect no marker (future-safe).
# -----------------------------------------------------------------------
step "scenario C: shim reports 2.52.0-rc1"
build_shim "2.52.0-rc1"
set +e
output_c=$(PATH="$tmp/bin:$PATH" bash -c "source '$extract'; check_git_version_known_bad" 2>&1)
rc_c=$?
set -e
echo "=== scenario C (git 2.52.0-rc1) stderr ===" >&3
echo "$output_c" >&3
echo "=== exit: $rc_c ===" >&3

if [ "$rc_c" -ne 0 ] || grep -q 'AM_INSTALL_WARN GIT_2_51_0_KNOWN_BAD' <<<"$output_c"; then
    echo "FAIL: 2.52.0-rc1 shim should NOT have flagged" >&2
    exit 1
fi
step "scenario C PASSED: no marker for 2.52.0-rc1"

# -----------------------------------------------------------------------
# Scenario D: no git on PATH (command -v returns nothing) → skip cleanly.
# Use a PATH that contains only system dirs without a git binary.
# -----------------------------------------------------------------------
step "scenario D: no git on PATH"
# Build an isolated PATH with bash/echo/awk but NO git.
no_git_path="$tmp/pathless"
mkdir -p "$no_git_path"
for prog in bash awk sh command head; do
    src=$(command -v "$prog" 2>/dev/null || true)
    [ -n "$src" ] && ln -sf "$src" "$no_git_path/$prog"
done
set +e
output_d=$(PATH="$no_git_path" "$no_git_path/bash" -c "source '$extract'; check_git_version_known_bad" 2>&1)
rc_d=$?
set -e
echo "=== scenario D (no git) stderr ===" >&3
echo "$output_d" >&3
echo "=== exit: $rc_d ===" >&3

if [ "$rc_d" -ne 0 ]; then
    echo "FAIL: no-git scenario should exit 0, got $rc_d" >&2
    echo "output: $output_d" >&2
    exit 1
fi

if grep -q 'AM_INSTALL_WARN' <<<"$output_d"; then
    echo "FAIL: no-git scenario unexpectedly emitted warning" >&2
    exit 1
fi
step "scenario D PASSED: clean skip when no git"

# -----------------------------------------------------------------------
# Scenario E: Git for Windows — "git version 2.51.0.windows.1" → warn.
# -----------------------------------------------------------------------
step "scenario E: shim reports 2.51.0.windows.1"
build_shim "2.51.0.windows.1"
set +e
output_e=$(PATH="$tmp/bin:$PATH" bash -c "source '$extract'; check_git_version_known_bad" 2>&1)
rc_e=$?
set -e
echo "=== scenario E (git 2.51.0.windows.1) ===" >&3
echo "$output_e" >&3

if [ "$rc_e" -ne 0 ]; then
    echo "FAIL: 2.51.0.windows.1 scenario exited $rc_e" >&2
    exit 1
fi
if ! grep -q 'AM_INSTALL_WARN GIT_2_51_0_KNOWN_BAD' <<<"$output_e"; then
    echo "FAIL: 2.51.0.windows.1 should have matched the known-bad glob" >&2
    echo "Captured:" >&2
    echo "$output_e" >&2
    exit 1
fi
step "scenario E PASSED: .windows.1 derivative flagged"

# -----------------------------------------------------------------------
# Scenario F: Ubuntu package suffix — "git version 2.51.0-1ubuntu1" → warn.
# -----------------------------------------------------------------------
step "scenario F: shim reports 2.51.0-1ubuntu1"
build_shim "2.51.0-1ubuntu1"
set +e
output_f=$(PATH="$tmp/bin:$PATH" bash -c "source '$extract'; check_git_version_known_bad" 2>&1)
rc_f=$?
set -e
echo "=== scenario F (git 2.51.0-1ubuntu1) ===" >&3
echo "$output_f" >&3

if [ "$rc_f" -ne 0 ]; then
    echo "FAIL: 2.51.0-1ubuntu1 scenario exited $rc_f" >&2
    exit 1
fi
if ! grep -q 'AM_INSTALL_WARN GIT_2_51_0_KNOWN_BAD' <<<"$output_f"; then
    echo "FAIL: 2.51.0-1ubuntu1 should have matched the known-bad glob" >&2
    echo "Captured:" >&2
    echo "$output_f" >&2
    exit 1
fi
step "scenario F PASSED: -1ubuntu1 derivative flagged"

# -----------------------------------------------------------------------
# Scenario G: 2.51.1 (post-fix version) → no warn.
# -----------------------------------------------------------------------
step "scenario G: shim reports 2.51.1 (post-fix)"
build_shim "2.51.1"
set +e
output_g=$(PATH="$tmp/bin:$PATH" bash -c "source '$extract'; check_git_version_known_bad" 2>&1)
rc_g=$?
set -e
echo "=== scenario G (git 2.51.1) ===" >&3
echo "$output_g" >&3

if [ "$rc_g" -ne 0 ]; then
    echo "FAIL: 2.51.1 scenario exited $rc_g" >&2
    exit 1
fi
if grep -q 'AM_INSTALL_WARN GIT_2_51_0_KNOWN_BAD' <<<"$output_g"; then
    echo "FAIL: 2.51.1 should NOT have been flagged (post-fix version)" >&2
    echo "Captured:" >&2
    echo "$output_g" >&2
    exit 1
fi
step "scenario G PASSED: 2.51.1 correctly not flagged"

step "ALL SCENARIOS PASSED"
echo "OK" >&3
