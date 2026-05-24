#!/usr/bin/env bash
#
# Verifies that install.sh selects the Linux GNU artifact during network
# preflight when the preferred x86_64 MUSL asset is absent but the GNU asset is
# available. This keeps ACFS and direct installs from warning on releases that
# only publish GNU Linux artifacts.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

if [ ! -f "$INSTALL_SH" ]; then
    echo "FATAL: $INSTALL_SH not found" >&2
    exit 2
fi

ts() { date -u +%Y-%m-%dT%H:%M:%SZ; }
step() { echo "[ARTIFACT_FALLBACK_TEST $(ts)] $*" >&2; }

tmp="$(mktemp -d)"
cleanup_tmp() {
    if command -v python3 >/dev/null 2>&1; then
        python3 - "$tmp" <<'PY' || true
import pathlib
import shutil
import sys

path = pathlib.Path(sys.argv[1])
if path.exists() and path.is_dir():
    shutil.rmtree(path)
PY
    else
        step "retaining temporary directory without python cleanup: $tmp"
    fi
}
trap cleanup_tmp EXIT

extract="$tmp/install_artifact_functions.sh"
extract_function() {
    local fn="$1"
    sed -n "/^${fn}() {/,/^}/p" "$INSTALL_SH"
}

{
    echo 'QUIET=0'
    echo 'HAS_GUM=0'
    echo 'NO_GUM=1'
    echo 'VERBOSE=0'
    extract_function info
    extract_function warn
    echo 'verbose() { :; }'
    extract_function set_artifact_url
    extract_function artifact_url_for_target_ext
    extract_function artifact_url_for_target
    extract_function set_target_artifact_ext
    extract_function set_target_artifact
    extract_function linux_x86_64_gnu_fallback_allowed
    extract_function artifact_url_reachable
    extract_function artifact_target_fallback_allowed
    extract_function select_artifact_for_target_if_available
    extract_function select_current_target_artifact_if_available
    extract_function select_same_target_gzip_artifact
    extract_function select_linux_x86_64_gnu_artifact
    extract_function select_linux_x86_64_gnu_artifact_if_available
    extract_function remove_installer_lock_dir
    extract_function check_network
} >"$extract"

for required in set_artifact_url check_network select_linux_x86_64_gnu_artifact_if_available; do
    if ! grep -q "^${required}()" "$extract"; then
        echo "FATAL: could not extract ${required} from install.sh" >&2
        exit 2
    fi
done

mkdir -p "$tmp/bin"
cat >"$tmp/bin/curl" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
url="${*: -1}"
printf '%s\n' "$url" >>"${CURL_LOG:?}"
case "$url" in
    *x86_64-unknown-linux-musl.tar.xz)
        exit "${MUSL_XZ_RC:-${MUSL_RC:-22}}"
        ;;
    *x86_64-unknown-linux-musl.tar.gz)
        exit "${MUSL_GZ_RC:-22}"
        ;;
    *x86_64-unknown-linux-gnu.tar.xz)
        exit "${GNU_XZ_RC:-${GNU_RC:-0}}"
        ;;
    *x86_64-unknown-linux-gnu.tar.gz)
        exit "${GNU_GZ_RC:-22}"
        ;;
    *custom.tar.xz)
        exit "${CUSTOM_RC:-22}"
        ;;
    *custom.tar.gz)
        exit "${CUSTOM_RC:-22}"
        ;;
    *)
        exit 0
        ;;
esac
SHIM
chmod +x "$tmp/bin/curl"

run_case() {
    local name="$1"
    shift
    local curl_log="$tmp/${name}.curl.log"
    : >"$curl_log"
    MUSL_RC="${MUSL_RC:-}" GNU_RC="${GNU_RC:-}" CUSTOM_RC="${CUSTOM_RC:-}" \
        CURL_LOG="$curl_log" PATH="$tmp/bin:$PATH" "$@" >"$tmp/${name}.out" 2>&1
    cat "$tmp/${name}.out"
}

step "scenario A: missing MUSL but reachable GNU switches without warning"
output_a=$(
    MUSL_XZ_RC=22 MUSL_GZ_RC=22 GNU_XZ_RC=22 GNU_GZ_RC=0 run_case scenario_a bash -c "
        source '$extract'
        OWNER=Dicklesworthstone
        REPO=mcp_agent_mail_rust
        VERSION=v0.2.51
        FROM_SOURCE=0
        OFFLINE=0
        ARTIFACT_URL=''
        TARGET=x86_64-unknown-linux-musl
        set_artifact_url
        check_network
        printf 'TARGET=%s\nURL=%s\n' \"\$TARGET\" \"\$URL\"
    "
)

if ! grep -q 'TARGET=x86_64-unknown-linux-gnu' <<<"$output_a"; then
    echo "FAIL: expected check_network to switch TARGET to GNU" >&2
    echo "$output_a" >&2
    exit 1
fi
if ! grep -q 'URL=.*x86_64-unknown-linux-gnu.tar.gz' <<<"$output_a"; then
    echo "FAIL: expected check_network to select the GNU .tar.gz artifact" >&2
    echo "$output_a" >&2
    exit 1
fi
if grep -q 'Network check failed' <<<"$output_a"; then
    echo "FAIL: GNU fallback should avoid the network warning" >&2
    echo "$output_a" >&2
    exit 1
fi
if ! grep -q 'x86_64-unknown-linux-musl.tar.xz' "$tmp/scenario_a.curl.log"; then
    echo "FAIL: scenario A did not probe the preferred MUSL artifact" >&2
    exit 1
fi
if ! grep -q 'x86_64-unknown-linux-gnu.tar.gz' "$tmp/scenario_a.curl.log"; then
    echo "FAIL: scenario A did not probe the GNU gzip fallback artifact" >&2
    exit 1
fi

step "scenario B: reachable MUSL keeps MUSL target and does not probe GNU"
output_b=$(
    MUSL_XZ_RC=0 GNU_XZ_RC=0 GNU_GZ_RC=0 run_case scenario_b bash -c "
        source '$extract'
        OWNER=Dicklesworthstone
        REPO=mcp_agent_mail_rust
        VERSION=v0.2.51
        FROM_SOURCE=0
        OFFLINE=0
        ARTIFACT_URL=''
        TARGET=x86_64-unknown-linux-musl
        set_artifact_url
        check_network
        printf 'TARGET=%s\nURL=%s\n' \"\$TARGET\" \"\$URL\"
    "
)

if ! grep -q 'TARGET=x86_64-unknown-linux-musl' <<<"$output_b"; then
    echo "FAIL: reachable MUSL artifact should keep the MUSL target" >&2
    echo "$output_b" >&2
    exit 1
fi
if grep -q 'x86_64-unknown-linux-gnu.tar.xz' "$tmp/scenario_b.curl.log"; then
    echo "FAIL: scenario B unexpectedly probed GNU when MUSL was reachable" >&2
    exit 1
fi
if grep -q 'x86_64-unknown-linux-gnu.tar.gz' "$tmp/scenario_b.curl.log"; then
    echo "FAIL: scenario B unexpectedly probed GNU gzip when MUSL was reachable" >&2
    exit 1
fi

step "scenario C: same-target .tar.gz is preferred before switching target"
output_c=$(
    MUSL_XZ_RC=22 MUSL_GZ_RC=0 GNU_XZ_RC=0 GNU_GZ_RC=0 run_case scenario_c bash -c "
        source '$extract'
        OWNER=Dicklesworthstone
        REPO=mcp_agent_mail_rust
        VERSION=v0.3.4
        FROM_SOURCE=0
        OFFLINE=0
        ARTIFACT_URL=''
        TARGET=x86_64-unknown-linux-musl
        set_artifact_url
        check_network
        printf 'TARGET=%s\nURL=%s\n' \"\$TARGET\" \"\$URL\"
    "
)

if ! grep -q 'TARGET=x86_64-unknown-linux-musl' <<<"$output_c"; then
    echo "FAIL: same-target gzip fallback should keep the MUSL target" >&2
    echo "$output_c" >&2
    exit 1
fi
if ! grep -q 'URL=.*x86_64-unknown-linux-musl.tar.gz' <<<"$output_c"; then
    echo "FAIL: expected same-target .tar.gz artifact" >&2
    echo "$output_c" >&2
    exit 1
fi
if grep -q 'x86_64-unknown-linux-gnu' "$tmp/scenario_c.curl.log"; then
    echo "FAIL: same-target gzip fallback should not probe GNU" >&2
    exit 1
fi

step "scenario D: explicit ARTIFACT_URL never falls back"
output_d=$(
    CUSTOM_RC=22 run_case scenario_d bash -c "
        source '$extract'
        OWNER=Dicklesworthstone
        REPO=mcp_agent_mail_rust
        VERSION=v0.2.51
        FROM_SOURCE=0
        OFFLINE=0
        ARTIFACT_URL='https://example.invalid/custom.tar.xz'
        TARGET=x86_64-unknown-linux-musl
        set_artifact_url
        check_network
        printf 'TARGET=%s\nURL=%s\n' \"\$TARGET\" \"\$URL\"
    "
)

if ! grep -q 'Network check failed for https://example.invalid/custom.tar.xz' <<<"$output_d"; then
    echo "FAIL: explicit artifact URL failure should still warn" >&2
    echo "$output_d" >&2
    exit 1
fi
if grep -q 'x86_64-unknown-linux-gnu.tar.xz' "$tmp/scenario_d.curl.log"; then
    echo "FAIL: explicit artifact URL should not probe GNU fallback" >&2
    exit 1
fi
if grep -q 'x86_64-unknown-linux-gnu.tar.gz' "$tmp/scenario_d.curl.log"; then
    echo "FAIL: explicit artifact URL should not probe GNU gzip fallback" >&2
    exit 1
fi

step "scenario E: stale installer lock cleanup uses pid-file removal plus rmdir"
lock_dir="$tmp/mcp-agent-mail-install.lock.d"
mkdir "$lock_dir"
echo 999999 >"$lock_dir/pid"
bash -c "source '$extract'; remove_installer_lock_dir '$lock_dir'"
if [ -e "$lock_dir" ]; then
    echo "FAIL: expected stale lock directory to be removed" >&2
    exit 1
fi

if grep -q 'rm -rf "${LOCK_DIR' "$INSTALL_SH" || grep -q 'rm -rf "$LOCK_DIR' "$INSTALL_SH"; then
    echo "FAIL: installer lock cleanup must not use rm -rf" >&2
    exit 1
fi

step "ALL SCENARIOS PASSED"
