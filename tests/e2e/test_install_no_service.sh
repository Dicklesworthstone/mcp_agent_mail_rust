#!/usr/bin/env bash
# test_install_no_service.sh — GH#243 regression harness for install.sh
#
# Proves the fail-safe service-management semantics:
#   * --dest outside the default install locations => the installer must not
#     touch systemd units at all (no unit write, no enable, no restart,
#     no daemon-reload).
#   * --no-service => same, even for a default-location install.
#   * default-location installs keep current behavior (service management runs).
#
# Method: extract the gate functions from install.sh verbatim, put a fake
# `systemctl` (and `crontab`) shim first on PATH that records every invocation,
# then drive the extracted functions and assert on the capture log.
#
# shellcheck disable=SC2034  # DEST/NO_SERVICE/PYTHON_PID/OS are read by the
#                            # functions eval'd out of install.sh, which
#                            # shellcheck cannot see.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
INSTALL_SH="${REPO_ROOT}/install.sh"

[ -f "$INSTALL_SH" ] || { echo "FATAL: install.sh not found at ${INSTALL_SH}" >&2; exit 1; }

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/am-no-service-harness.XXXXXX")"
trap 'rm -rf "$WORKDIR"' EXIT
FAKEBIN="${WORKDIR}/fakebin"
CAPTURE="${WORKDIR}/systemctl-calls.log"
mkdir -p "$FAKEBIN"
: > "$CAPTURE"

# --- fake systemctl: records every invocation, reports services as active ---
cat > "${FAKEBIN}/systemctl" <<EOF
#!/usr/bin/env bash
printf 'systemctl %s\n' "\$*" >> "${CAPTURE}"
case "\$1" in
  --user)
    case "\$2" in
      is-active) exit 0 ;;
    esac
    ;;
esac
exit 0
EOF
chmod +x "${FAKEBIN}/systemctl"

# --- fake crontab: empty crontab, records invocations (stop_python_server uses it) ---
cat > "${FAKEBIN}/crontab" <<EOF
#!/usr/bin/env bash
printf 'crontab %s\n' "\$*" >> "${CAPTURE}"
exit 0
EOF
chmod +x "${FAKEBIN}/crontab"

# --- fake pgrep: reports no matches, so stop_python_server's Python-process
# kill sweep is inert inside the harness (it must never touch real processes,
# and `pgrep -f mcp_agent_mail` would otherwise self-match the harness shell).
cat > "${FAKEBIN}/pgrep" <<EOF
#!/usr/bin/env bash
printf 'pgrep %s\n' "\$*" >> "${CAPTURE}"
exit 1
EOF
chmod +x "${FAKEBIN}/pgrep"

export PATH="${FAKEBIN}:${PATH}"

# --- extract the gate functions verbatim from install.sh ---
extract_fn() {
  local name="$1"
  local body
  body="$(sed -n "/^${name}()/,/^}/p" "$INSTALL_SH")"
  if [ -z "$body" ]; then
    echo "FATAL: could not extract function ${name} from install.sh" >&2
    exit 1
  fi
  eval "$body"
}

# minimal logging stubs used by the extracted functions
info() { printf 'info: %s\n' "$*"; }
warn() { printf 'warn: %s\n' "$*"; }
ok() { printf 'ok: %s\n' "$*"; }
err() { printf 'err: %s\n' "$*" >&2; }
verbose() { :; }

extract_fn dest_is_default_install_location
extract_fn service_management_allowed
extract_fn stop_python_server

SERVICE_MANAGEMENT_SKIP_REASON=""
DEST_DEFAULT="${HOME}/.local/bin"
# install.sh sets these before stop_python_server runs; mirror that here.
PYTHON_PID=""
OS="linux"

PASS=0
FAIL=0
assert() {
  local desc="$1"; shift
  if "$@"; then
    PASS=$((PASS + 1))
    echo "PASS: ${desc}"
  else
    FAIL=$((FAIL + 1))
    echo "FAIL: ${desc}" >&2
  fi
}

systemctl_call_count() { grep -c '^systemctl ' "$CAPTURE" || true; }

# ---------------------------------------------------------------- gate logic
DEST="${WORKDIR}/scratch/bin"; NO_SERVICE=0
if service_management_allowed; then
  FAIL=$((FAIL + 1)); echo "FAIL: scratch --dest must block service management" >&2
else
  PASS=$((PASS + 1)); echo "PASS: scratch --dest blocks service management"
  case "$SERVICE_MANAGEMENT_SKIP_REASON" in
    *non-default*) PASS=$((PASS + 1)); echo "PASS: skip reason names non-default dest" ;;
    *) FAIL=$((FAIL + 1)); echo "FAIL: skip reason wrong: ${SERVICE_MANAGEMENT_SKIP_REASON}" >&2 ;;
  esac
fi

DEST="${DEST_DEFAULT}"; NO_SERVICE=1
if service_management_allowed; then
  FAIL=$((FAIL + 1)); echo "FAIL: --no-service must block service management" >&2
else
  PASS=$((PASS + 1)); echo "PASS: --no-service blocks service management"
  case "$SERVICE_MANAGEMENT_SKIP_REASON" in
    *--no-service*) PASS=$((PASS + 1)); echo "PASS: skip reason names --no-service" ;;
    *) FAIL=$((FAIL + 1)); echo "FAIL: skip reason wrong: ${SERVICE_MANAGEMENT_SKIP_REASON}" >&2 ;;
  esac
fi

DEST="${DEST_DEFAULT}"; NO_SERVICE=0
assert "default dest allows service management" service_management_allowed

DEST="${DEST_DEFAULT}/"; NO_SERVICE=0
assert "default dest with trailing slash allows service management" service_management_allowed

DEST="/usr/local/bin"; NO_SERVICE=0
assert "--system dest (/usr/local/bin) allows service management" service_management_allowed

# ------------------------------------------- systemctl invocation capture
# Scratch dest: stop_python_server must make ZERO systemctl calls.
: > "$CAPTURE"
DEST="${WORKDIR}/scratch/bin"; NO_SERVICE=0
stop_python_server >/dev/null 2>&1 || true
count="$(systemctl_call_count)"
assert "zero systemctl invocations under --dest scratch (saw ${count})" test "${count}" -eq 0

# --no-service at default dest: also zero systemctl calls.
: > "$CAPTURE"
DEST="${DEST_DEFAULT}"; NO_SERVICE=1
stop_python_server >/dev/null 2>&1 || true
count="$(systemctl_call_count)"
assert "zero systemctl invocations under --no-service (saw ${count})" test "${count}" -eq 0

# Negative control: default dest WITHOUT --no-service must hit systemctl
# (proves the fake shim actually captures, so the zero-counts above are real).
: > "$CAPTURE"
DEST="${DEST_DEFAULT}"; NO_SERVICE=0
stop_python_server >/dev/null 2>&1 || true
count="$(systemctl_call_count)"
assert "default install still manages services (saw ${count} systemctl calls)" test "${count}" -gt 0

# ------------------------------------------------------------- announcement
# The service-management step must announce the target unit before acting.
assert "installer announces the unit before touching it" \
  grep -q 'About to install/enable/restart' "$INSTALL_SH"
assert "launchd repair announces the plist before rewriting it" \
  grep -q 'About to rewrite LaunchAgent plist' "$INSTALL_SH"

# --no-service must be a documented flag wired to NO_SERVICE=1
assert "--no-service flag is parsed" grep -q -- '--no-service) NO_SERVICE=1' "$INSTALL_SH"

echo
echo "test_install_no_service: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
