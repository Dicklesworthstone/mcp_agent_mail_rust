#!/usr/bin/env bash
# test_self_update.sh - E2E suite for self-update flow using mocked release endpoints.
#
# Covers:
# - `am self-update --check` (mocked API says current is latest)
# - `am self-update --version <current>` (explicit reinstall path)
# - `am self-update --force` (force reinstall current version)
#
# This suite is network-independent by serving release metadata/artifacts from a
# local HTTP server and overriding self-update URLs via environment variables.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
export E2E_SUITE="self_update"

# shellcheck source=e2e_lib.sh
source "${SCRIPT_DIR}/../../scripts/e2e_lib.sh"

e2e_init_artifacts
e2e_banner "Self-Update E2E Suite (br-28mgh.8.6)"

WORK="$(e2e_mktemp "e2e_self_update")"
FAKE_HOME="${WORK}/home"
INSTALL_DIR="${FAKE_HOME}/.local/bin"
MOCK_ROOT="${WORK}/mock_release"
CACHE_HOME="${WORK}/cache"
HTTP_LOG="${WORK}/mock_http_server.log"
mkdir -p "${INSTALL_DIR}" "${MOCK_ROOT}" "${CACHE_HOME}"

cleanup_self_update() {
    if [ -n "${HTTP_PID:-}" ]; then
        kill "${HTTP_PID}" 2>/dev/null || true
        wait "${HTTP_PID}" 2>/dev/null || true
    fi
}
trap cleanup_self_update EXIT

detect_target_triple() {
    local os arch
    os="$(uname -s | tr '[:upper:]' '[:lower:]')"
    arch="$(uname -m)"
    case "${os}:${arch}" in
        linux:x86_64) echo "x86_64-unknown-linux-gnu" ;;
        linux:aarch64) echo "aarch64-unknown-linux-gnu" ;;
        darwin:x86_64) echo "x86_64-apple-darwin" ;;
        darwin:arm64) echo "aarch64-apple-darwin" ;;
        msys*:x86_64|mingw*:x86_64|cygwin*:x86_64) echo "x86_64-pc-windows-msvc" ;;
        *) return 1 ;;
    esac
}

e2e_ensure_binary "am" >/dev/null
e2e_ensure_binary "mcp-agent-mail" >/dev/null
CLI_BIN="${CARGO_TARGET_DIR}/debug/am"
SERVER_BIN="${CARGO_TARGET_DIR}/debug/mcp-agent-mail"

reset_install_binaries() {
    cp "${CLI_BIN}" "${INSTALL_DIR}/am"
    cp "${SERVER_BIN}" "${INSTALL_DIR}/mcp-agent-mail"
    chmod +x "${INSTALL_DIR}/am" "${INSTALL_DIR}/mcp-agent-mail"
    rm -rf "${CACHE_HOME}/mcp-agent-mail" 2>/dev/null || true
}

TARGET_TRIPLE="$(detect_target_triple || true)"
if [ -z "${TARGET_TRIPLE}" ]; then
    e2e_case_banner "platform target detection"
    e2e_mark_case_start "case01_platform_target_detection"
    e2e_skip "Unsupported platform for self-update artifact naming: $(uname -s)/$(uname -m)"
    e2e_summary
    exit 0
fi

if [[ "${TARGET_TRIPLE}" == *windows* ]]; then
    e2e_case_banner "platform target format"
    e2e_mark_case_start "case02_platform_target_format"
    e2e_skip "Windows archive format (.zip) is not covered by this Linux/macOS E2E harness."
    e2e_summary
    exit 0
fi

reset_install_binaries

CURRENT_VERSION="$("${INSTALL_DIR}/am" --version 2>/dev/null | awk '{print $2}' | sed 's/^v//' | head -1)"
if [ -z "${CURRENT_VERSION}" ]; then
    CURRENT_VERSION="$(echo "${CARGO_PKG_VERSION:-}" | sed 's/^v//')"
fi
if [ -z "${CURRENT_VERSION}" ]; then
    CURRENT_VERSION="0.0.0"
fi

ASSET_NAME="mcp-agent-mail-${TARGET_TRIPLE}.tar.xz"
ASSET_DIR="${MOCK_ROOT}/releases/download/v${CURRENT_VERSION}"
ASSET_PATH="${ASSET_DIR}/${ASSET_NAME}"
mkdir -p "${ASSET_DIR}"

# Include deterministic incompressible padding so the mocked tarball exceeds
# the old asupersync non-streaming 16 MiB body limit. That keeps this E2E tied
# to the streaming self-update path instead of only proving tiny artifact
# replacement.
PAYLOAD_STAGE="${WORK}/payload"
mkdir -p "${PAYLOAD_STAGE}"
cat > "${PAYLOAD_STAGE}/am" <<EOF
#!/usr/bin/env bash
set -euo pipefail
VERSION="${CURRENT_VERSION}"
cmd="\${1:-}"
sub="\${2:-}"
case "\$cmd" in
  --version|-V|version)
    echo "am \${VERSION}"
    ;;
  self-update)
    case "\$sub" in
      --check)
        echo "Up to date (v\${VERSION})"
        ;;
      --force)
        echo "Force reinstalling v\${VERSION}"
        echo "Downloaded and verified v\${VERSION}"
        echo "Update complete (v\${VERSION} -> v\${VERSION}). Restart am to use the new version."
        ;;
      --version)
        target="\${3:-\${VERSION}}"
        target="\${target#v}"
        echo "Installing version \${target} (current: \${VERSION})"
        echo "Downloaded and verified v\${target}"
        echo "Update complete (v\${VERSION} -> v\${target}). Restart am to use the new version."
        ;;
      *)
        echo "Up to date (v\${VERSION})"
        ;;
    esac
    ;;
  *)
    echo "am \${VERSION}"
    ;;
esac
EOF
chmod +x "${PAYLOAD_STAGE}/am"

cat > "${PAYLOAD_STAGE}/mcp-agent-mail" <<EOF
#!/usr/bin/env bash
set -euo pipefail
echo "mcp-agent-mail ${CURRENT_VERSION}"
EOF
chmod +x "${PAYLOAD_STAGE}/mcp-agent-mail"

STREAMING_SENTINEL_SIZE=$((17 * 1024 * 1024))
python3 - "${PAYLOAD_STAGE}/streaming-sentinel.bin" "${STREAMING_SENTINEL_SIZE}" <<'PY'
import hashlib
import sys

path = sys.argv[1]
remaining = int(sys.argv[2])
counter = 0

with open(path, "wb") as fh:
    while remaining > 0:
        block = hashlib.sha256(counter.to_bytes(8, "little")).digest()
        size = min(len(block), remaining)
        fh.write(block[:size])
        remaining -= size
        counter += 1
PY

tar -cJf "${ASSET_PATH}" -C "${PAYLOAD_STAGE}" am mcp-agent-mail streaming-sentinel.bin
ASSET_BYTES="$(wc -c < "${ASSET_PATH}" | tr -d '[:space:]')"
OLD_HTTP_BODY_LIMIT_BYTES=$((16 * 1024 * 1024))
e2e_case_banner "mock release asset exceeds streaming threshold"
e2e_save_artifact "mock_asset_size.txt" "asset_bytes=${ASSET_BYTES}
old_http_body_limit_bytes=${OLD_HTTP_BODY_LIMIT_BYTES}
streaming_sentinel_bytes=${STREAMING_SENTINEL_SIZE}"
if [ "${ASSET_BYTES}" -gt "${OLD_HTTP_BODY_LIMIT_BYTES}" ]; then
    e2e_pass "mock tarball exceeds old non-streaming body limit (${ASSET_BYTES} bytes)"
else
    e2e_fail "mock tarball should exceed old non-streaming body limit (${ASSET_BYTES} bytes)"
    e2e_summary
    exit 1
fi

ASSET_SHA="$(e2e_sha256 "${ASSET_PATH}")"
printf "%s  %s\n" "${ASSET_SHA}" "${ASSET_NAME}" > "${ASSET_PATH}.sha256"

if command -v python3 >/dev/null 2>&1; then
    PORT="$(python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
)"
else
    PORT="$((19000 + RANDOM % 1000))"
fi

API_DIR="${MOCK_ROOT}/repos/Dicklesworthstone/mcp_agent_mail_rust/releases"
mkdir -p "${API_DIR}"
cat > "${API_DIR}/latest" <<EOF
{
  "tag_name": "v${CURRENT_VERSION}",
  "html_url": "http://127.0.0.1:${PORT}/releases/v${CURRENT_VERSION}"
}
EOF

e2e_save_artifact "mock_latest.json" "$(cat "${API_DIR}/latest")"
e2e_save_artifact "mock_asset_sha256.txt" "${ASSET_SHA}"

(
    cd "${MOCK_ROOT}"
    python3 -m http.server "${PORT}" --bind 127.0.0.1 >"${HTTP_LOG}" 2>&1
) &
HTTP_PID=$!

if ! e2e_wait_port "127.0.0.1" "${PORT}" 20; then
    e2e_case_banner "mock release server startup"
    e2e_mark_case_start "case03_mock_release_server_startup"
    e2e_fail "Local mock HTTP server failed to start on port ${PORT}"
    e2e_save_artifact "mock_http_server.log" "$(cat "${HTTP_LOG}" 2>/dev/null || true)"
    e2e_summary
    exit 1
fi

API_URL="http://127.0.0.1:${PORT}/repos/Dicklesworthstone/mcp_agent_mail_rust/releases/latest"
RELEASE_BASE_URL="http://127.0.0.1:${PORT}/releases/download"

run_self_update_cmd() {
    local artifact_prefix="$1"
    shift
    local stdout_file="${WORK}/${artifact_prefix}_stdout.txt"
    local stderr_file="${WORK}/${artifact_prefix}_stderr.txt"

    set +e
    (
        HOME="${FAKE_HOME}" \
        PATH="${INSTALL_DIR}:/usr/bin:/bin" \
        XDG_CACHE_HOME="${CACHE_HOME}" \
        AM_SELF_UPDATE_API_URL="${API_URL}" \
        AM_SELF_UPDATE_RELEASES_BASE_URL="${RELEASE_BASE_URL}" \
        "${INSTALL_DIR}/am" "$@"
    ) >"${stdout_file}" 2>"${stderr_file}"
    LAST_RC=$?
    set -e

    LAST_STDOUT="$(cat "${stdout_file}" 2>/dev/null || true)"
    LAST_STDERR="$(cat "${stderr_file}" 2>/dev/null || true)"
    e2e_save_artifact "${artifact_prefix}_stdout.txt" "${LAST_STDOUT}"
    e2e_save_artifact "${artifact_prefix}_stderr.txt" "${LAST_STDERR}"
}

# ===========================================================================
# Case 1: --check reports up-to-date against mocked API
# ===========================================================================
e2e_case_banner "self-update --check against mocked API"
e2e_mark_case_start "case04_selfupdate_check_against_mocked_api"

run_self_update_cmd "case_01_check" self-update --check
e2e_assert_exit_code "self-update --check exits cleanly" "0" "${LAST_RC}"
e2e_assert_contains "check reports up-to-date current version" "${LAST_STDOUT}" "Up to date (v${CURRENT_VERSION})"
e2e_assert_not_contains "check stderr has no fatal error" "${LAST_STDERR}" "Update check failed:"

# ===========================================================================
# Case 2: explicit version reinstall works from mocked release asset
# ===========================================================================
e2e_case_banner "self-update --version current"
e2e_mark_case_start "case05_selfupdate_version_current"

reset_install_binaries
run_self_update_cmd "case_02_version" self-update --version "${CURRENT_VERSION}"
e2e_assert_exit_code "self-update --version exits cleanly" "0" "${LAST_RC}"
e2e_assert_contains "download verification output present" "${LAST_STDOUT}" "Downloaded and verified v${CURRENT_VERSION}"
e2e_assert_contains "update completion output present" "${LAST_STDOUT}" "Update complete"

set +e
VERSION_AFTER_VERSION_CMD="$("${INSTALL_DIR}/am" --version 2>&1)"
VERSION_AFTER_VERSION_RC=$?
set -e
e2e_assert_exit_code "am --version after explicit reinstall" "0" "${VERSION_AFTER_VERSION_RC}"
e2e_assert_contains "version remains current after explicit reinstall" "${VERSION_AFTER_VERSION_CMD}" "${CURRENT_VERSION}"

# ===========================================================================
# Case 3: --force reinstall works for current version
# ===========================================================================
e2e_case_banner "self-update --force current version"
e2e_mark_case_start "case06_selfupdate_force_current_version"

reset_install_binaries
run_self_update_cmd "case_03_force" self-update --force
e2e_assert_exit_code "self-update --force exits cleanly" "0" "${LAST_RC}"
e2e_assert_contains "force path announces current version" "${LAST_STDOUT}" "Force reinstalling v${CURRENT_VERSION}"
e2e_assert_contains "force path completes update" "${LAST_STDOUT}" "Update complete"

set +e
VERSION_AFTER_FORCE="$("${INSTALL_DIR}/am" --version 2>&1)"
VERSION_AFTER_FORCE_RC=$?
set -e
e2e_assert_exit_code "am --version after force reinstall" "0" "${VERSION_AFTER_FORCE_RC}"
e2e_assert_contains "version remains current after force reinstall" "${VERSION_AFTER_FORCE}" "${CURRENT_VERSION}"

# ===========================================================================
# Case 4: repeat --check remains stable and actionable
# ===========================================================================
e2e_case_banner "repeat self-update --check stability"
e2e_mark_case_start "case07_repeat_selfupdate_check_stability"

run_self_update_cmd "case_04_check_repeat" self-update --check
e2e_assert_exit_code "repeat self-update --check exits cleanly" "0" "${LAST_RC}"
e2e_assert_contains "repeat check remains up-to-date" "${LAST_STDOUT}" "Up to date (v${CURRENT_VERSION})"

e2e_save_artifact "mock_http_server.log" "$(cat "${HTTP_LOG}" 2>/dev/null || true)"

e2e_summary
