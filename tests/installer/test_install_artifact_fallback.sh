#!/usr/bin/env bash
#
# Verifies artifact fallback plus the fail-closed release-evidence contract
# shared by the Unix and Windows installers: exact tag identity, checksum and
# Sigstore witnesses, exact archive inventory, and exact staged/post-install
# binary versions.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"
INSTALL_PS1="$REPO_ROOT/install.ps1"

if [ ! -f "$INSTALL_SH" ] || [ ! -f "$INSTALL_PS1" ]; then
    echo "FATAL: installer source not found ($INSTALL_SH or $INSTALL_PS1)" >&2
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
    echo 'VERBOSE_DUMP_LINES=20'
    echo 'LOG_FILE=/tmp/unused-installer-verification-test.log'
    echo 'ISSUES_URL=https://example.invalid/issues'
    echo 'COSIGN_BIN='
    extract_function info
    extract_function ok
    extract_function warn
    extract_function err
    extract_function error_support_hint
    echo 'verbose() { :; }'
    extract_function establish_release_contract
    extract_function capture_command_with_timeout
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
    extract_function persist_installer_copy
    extract_function verify_checksum
    extract_function resolve_and_verify_archive_checksum
    extract_function require_safe_cosign
    extract_function verify_sigstore_bundle
    extract_function verify_release_archive
    extract_function verify_archive_members_exact
    extract_function binary_version_matches_exact
    extract_function verify_release_binaries_exact
    extract_function ensure_real_directory_tree
    extract_function ensure_real_file_target_path
    extract_function file_sha256_hex
    extract_function rollback_binary_install_target
    extract_function rollback_binary_pair_install
    extract_function install_binary_pair_transactional
} >"$extract"

for required in establish_release_contract set_artifact_url check_network \
    select_linux_x86_64_gnu_artifact_if_available verify_release_archive \
    verify_archive_members_exact verify_release_binaries_exact require_safe_cosign \
    install_binary_pair_transactional; do
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
    *installer-source.sh)
        printf '%s\n' '#!/usr/bin/env bash' '#' '# mcp-agent-mail installer' 'echo persisted-installer'
        exit 0
        ;;
    *invalid-installer.sh)
        printf '%s\n' '<html>not an installer</html>'
        exit 0
        ;;
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

cat >"$tmp/bin/cosign" <<'SHIM'
#!/usr/bin/env bash
set -euo pipefail
if [ "${1:-}" = "version" ]; then
    printf '%s\n' "${COSIGN_VERSION_OUTPUT:-GitVersion: v3.1.3}"
    exit 0
fi
for trust_name in SIGSTORE_ROOT_FILE SIGSTORE_REKOR_PUBLIC_KEY SIGSTORE_CT_LOG_PUBLIC_KEY_FILE; do
    if [ -n "${!trust_name:-}" ]; then
        echo "custom trust environment leaked into cosign: $trust_name" >&2
        exit 43
    fi
done
if [ -n "${COSIGN_LOG:-}" ]; then
    printf '%s\n' "$@" >>"$COSIGN_LOG"
fi

certificate_identity="${COSIGN_CERTIFICATE_IDENTITY:-}"
identity=""
identity_regexp=""
args=("$@")
for ((index = 0; index < ${#args[@]}; index++)); do
    case "${args[$index]}" in
        --certificate-identity)
            index=$((index + 1))
            identity="${args[$index]:-}"
            ;;
        --certificate-identity-regexp)
            index=$((index + 1))
            identity_regexp="${args[$index]:-}"
            ;;
    esac
done

if [ -n "$certificate_identity" ]; then
    if [ -n "$identity" ]; then
        [ "$certificate_identity" = "$identity" ] || exit 42
    elif [ -n "$identity_regexp" ]; then
        [[ "$certificate_identity" =~ $identity_regexp ]] || exit 42
    else
        exit 42
    fi
fi
exit "${COSIGN_VERIFY_RC:-0}"
SHIM
chmod +x "$tmp/bin/cosign"

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
        VERSION=v0.3.6
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

# shellcheck disable=SC2016 # grep patterns intentionally match literal shell source.
if grep -q 'rm -rf "${LOCK_DIR' "$INSTALL_SH" || grep -q 'rm -rf "$LOCK_DIR' "$INSTALL_SH"; then
    echo "FAIL: installer lock cleanup must not use rm -rf" >&2
    exit 1
fi

step "scenario F: piped installer persistence is complete-or-untouched"
piped_home="$tmp/piped-home"
mkdir -p "$piped_home"
{
    printf '%s\n' \
        'OFFLINE=0' \
        'INSTALL_SCRIPT_URL=https://example.invalid/installer-source.sh' \
        'verbose() { :; }'
    extract_function persist_installer_copy
    # shellcheck disable=SC2016 # generated script must expand this variable at runtime.
    printf '%s\n' 'persist_installer_copy' 'printf "%s\n" "$SAVED_INSTALLER_PATH"'
} | HOME="$piped_home" CURL_LOG="$tmp/scenario_f.curl.log" PATH="$tmp/bin:$PATH" \
    bash >"$tmp/scenario_f.out"

saved_installer="$piped_home/.local/share/mcp-agent-mail/install.sh"
if [ ! -x "$saved_installer" ]; then
    echo "FAIL: piped install did not persist an executable installer" >&2
    exit 1
fi
if ! grep -q '^echo persisted-installer$' "$saved_installer"; then
    echo "FAIL: persisted installer did not contain the complete fetched payload" >&2
    exit 1
fi
if ! grep -Fxq "$saved_installer" "$tmp/scenario_f.out"; then
    echo "FAIL: persisted installer path was not reported" >&2
    exit 1
fi

printf '%s\n' 'known-good-installer' >"$saved_installer"
{
    printf '%s\n' \
        'OFFLINE=0' \
        'INSTALL_SCRIPT_URL=https://example.invalid/custom.tar.xz' \
        'verbose() { :; }'
    extract_function persist_installer_copy
    printf '%s\n' 'persist_installer_copy'
} | HOME="$piped_home" CURL_LOG="$tmp/scenario_f_failed.curl.log" CUSTOM_RC=22 \
    PATH="$tmp/bin:$PATH" bash
if [ "$(cat "$saved_installer")" != "known-good-installer" ]; then
    echo "FAIL: failed installer fetch modified the existing saved copy" >&2
    exit 1
fi

{
    printf '%s\n' \
        'OFFLINE=0' \
        'INSTALL_SCRIPT_URL=https://example.invalid/invalid-installer.sh' \
        'verbose() { :; }'
    extract_function persist_installer_copy
    printf '%s\n' 'persist_installer_copy'
} | HOME="$piped_home" CURL_LOG="$tmp/scenario_f_invalid.curl.log" \
    PATH="$tmp/bin:$PATH" bash
if [ "$(cat "$saved_installer")" != "known-good-installer" ]; then
    echo "FAIL: invalid successful response modified the existing saved copy" >&2
    exit 1
fi

step "scenario G: release archive verification is fail-closed and precedes extraction"
verification_harness="$tmp/verification_harness.sh"
cat >"$verification_harness" <<'HARNESS'
#!/usr/bin/env bash
set -euo pipefail
source "${EXTRACT:?}"

TMP="${CASE_TMP:?}"
CHECKSUM="${CHECKSUM_OVERRIDE:-}"
CHECKSUM_URL=""
SIGSTORE_BUNDLE_URL=""
COSIGN_OIDC_ISSUER='https://token.actions.githubusercontent.com'
VERSION="${REQUESTED_VERSION:-v9.9.9}"
establish_release_contract

download_to_file() {
    local _url="$1"
    local destination="$2"
    local label="$3"
    case "$label" in
        sha256sums-download)
            [ "${WITNESS_MODE:-valid}" = "missing_checksum" ] && return 1
            printf '%s  %s\n' "${ARCHIVE_SHA256:?}" "${ARTIFACT_NAME:?}" >"$destination"
            ;;
        checksum-download)
            return 1
            ;;
        sigstore-bundle)
            case "${WITNESS_MODE:-valid}" in
                missing_bundle) return 1 ;;
                empty_bundle) : >"$destination" ;;
                malformed_bundle) printf '%s\n' '{' >"$destination" ;;
                *) printf '%s\n' '{"mediaType":"application/vnd.dev.sigstore.bundle.v0.3+json"}' >"$destination" ;;
            esac
            ;;
        *) return 1 ;;
    esac
}

case "${ACTION:-archive}" in
    archive)
        verify_release_archive "${ARCHIVE_FILE:?}" "${ARTIFACT_URL:?}" "${ARTIFACT_NAME:?}"
        ;;
    checksum_only)
        verify_checksum "${ARCHIVE_FILE:?}" "${ARCHIVE_SHA256:?}"
        ;;
    sigstore_only)
        verify_sigstore_bundle "${ARCHIVE_FILE:?}" "${ARTIFACT_URL:?}"
        ;;
    *)
        echo "unknown verification action: ${ACTION}" >&2
        exit 2
        ;;
esac
HARNESS
chmod +x "$verification_harness"

archive_name="mcp-agent-mail-x86_64-unknown-linux-gnu.tar.xz"
archive_file="$tmp/$archive_name"
printf '%s\n' 'installer verification fixture' >"$archive_file"
if command -v sha256sum >/dev/null 2>&1; then
    archive_sha256=$(sha256sum "$archive_file" | awk '{print $1}')
else
    archive_sha256=$(shasum -a 256 "$archive_file" | awk '{print $1}')
fi
artifact_url="https://github.com/Dicklesworthstone/mcp_agent_mail_rust/releases/download/v9.9.9/$archive_name"

run_verification_case() {
    local name="$1"
    shift
    local case_tmp="$tmp/$name"
    mkdir -p "$case_tmp"
    EXTRACT="$extract" CASE_TMP="$case_tmp" ARCHIVE_FILE="$archive_file" \
        ARCHIVE_SHA256="$archive_sha256" ARTIFACT_NAME="$archive_name" \
        ARTIFACT_URL="$artifact_url" COSIGN_LOG="$tmp/$name.cosign.log" \
        CHECKSUM_OVERRIDE="${CHECKSUM_OVERRIDE:-}" WITNESS_MODE="${WITNESS_MODE:-valid}" \
        REQUESTED_VERSION="${REQUESTED_VERSION:-v9.9.9}" \
        COSIGN_VERSION_OUTPUT="${COSIGN_VERSION_OUTPUT:-GitVersion: v3.1.3}" \
        COSIGN_CERTIFICATE_IDENTITY="${COSIGN_CERTIFICATE_IDENTITY:-https://github.com/Dicklesworthstone/mcp_agent_mail_rust/.github/workflows/dist.yml@refs/tags/v9.9.9}" \
        COSIGN_VERIFY_RC="${COSIGN_VERIFY_RC:-0}" \
        SIGSTORE_ROOT_FILE="${SIGSTORE_ROOT_FILE:-}" \
        SIGSTORE_REKOR_PUBLIC_KEY="${SIGSTORE_REKOR_PUBLIC_KEY:-}" \
        SIGSTORE_CT_LOG_PUBLIC_KEY_FILE="${SIGSTORE_CT_LOG_PUBLIC_KEY_FILE:-}" \
        PATH="$tmp/bin:$PATH" "$@" "$verification_harness" \
        >"$tmp/$name.out" 2>&1
}

if WITNESS_MODE=missing_checksum run_verification_case verify_missing_checksum bash; then
    echo "FAIL: missing checksum witness must abort archive verification" >&2
    exit 1
fi
if [ -s "$tmp/verify_missing_checksum.cosign.log" ]; then
    echo "FAIL: signature verification ran after checksum witness failure" >&2
    exit 1
fi
if ! grep -q 'No SHA256 checksum witness is available' "$tmp/verify_missing_checksum.out"; then
    echo "FAIL: missing checksum failure was not actionable" >&2
    cat "$tmp/verify_missing_checksum.out" >&2
    exit 1
fi

if CHECKSUM_OVERRIDE='not-a-sha256-digest' WITNESS_MODE=valid \
    run_verification_case verify_invalid_checksum bash; then
    echo "FAIL: malformed checksum witness must abort archive verification" >&2
    exit 1
fi
if [ -s "$tmp/verify_invalid_checksum.cosign.log" ]; then
    echo "FAIL: signature verification ran after malformed checksum failure" >&2
    exit 1
fi
if ! grep -q 'Invalid SHA256 checksum witness' "$tmp/verify_invalid_checksum.out"; then
    echo "FAIL: malformed checksum witness failure was not actionable" >&2
    cat "$tmp/verify_invalid_checksum.out" >&2
    exit 1
fi

mkdir -p "$tmp/no-tools" "$tmp/verify_no_sha"
if EXTRACT="$extract" CASE_TMP="$tmp/verify_no_sha" ARCHIVE_FILE="$archive_file" \
    ARCHIVE_SHA256="$archive_sha256" ARTIFACT_NAME="$archive_name" \
    ARTIFACT_URL="$artifact_url" ACTION=checksum_only PATH="$tmp/no-tools" \
    /bin/bash "$verification_harness" >"$tmp/verify_no_sha.out" 2>&1; then
    echo "FAIL: missing SHA256 implementation must abort verification" >&2
    exit 1
fi
if ! grep -q 'No SHA256 implementation found' "$tmp/verify_no_sha.out"; then
    echo "FAIL: missing SHA256 implementation failure was not actionable" >&2
    exit 1
fi

for bundle_mode in missing_bundle empty_bundle; do
    if CHECKSUM_OVERRIDE="$archive_sha256" WITNESS_MODE="$bundle_mode" \
        run_verification_case "verify_$bundle_mode" bash; then
        echo "FAIL: $bundle_mode must abort archive verification" >&2
        exit 1
    fi
done

mkdir -p "$tmp/verify_no_cosign"
if EXTRACT="$extract" CASE_TMP="$tmp/verify_no_cosign" ARCHIVE_FILE="$archive_file" \
    ARCHIVE_SHA256="$archive_sha256" ARTIFACT_NAME="$archive_name" \
    ARTIFACT_URL="$artifact_url" ACTION=sigstore_only PATH="$tmp/no-tools" \
    /bin/bash "$verification_harness" >"$tmp/verify_no_cosign.out" 2>&1; then
    echo "FAIL: missing cosign must abort signature verification" >&2
    exit 1
fi
if ! grep -q 'cosign is required' "$tmp/verify_no_cosign.out"; then
    echo "FAIL: missing cosign failure was not actionable" >&2
    exit 1
fi

for unsafe_version_case in \
    'v3_1_2|GitVersion: v3.1.2' \
    'v2_6_5|GitVersion: v2.6.5' \
    'v4_0_0|GitVersion: v4.0.0' \
    'prerelease|GitVersion: v3.1.3-rc.1' \
    'multiple|GitVersion: v3.1.3
GitVersion: v3.2.0' \
    'malformed|cosign version 3.1.3'; do
    unsafe_name="${unsafe_version_case%%|*}"
    unsafe_output="${unsafe_version_case#*|}"
    if CHECKSUM_OVERRIDE="$archive_sha256" WITNESS_MODE=valid \
        COSIGN_VERSION_OUTPUT="$unsafe_output" \
        run_verification_case "verify_unsafe_cosign_$unsafe_name" bash; then
        echo "FAIL: unsafe or ambiguous cosign version was accepted: $unsafe_name" >&2
        exit 1
    fi
    if ! grep -Eq 'Unsafe or unsupported cosign version|Could not parse exactly one stable GitVersion' \
        "$tmp/verify_unsafe_cosign_$unsafe_name.out"; then
        echo "FAIL: cosign rejection was not actionable: $unsafe_name" >&2
        cat "$tmp/verify_unsafe_cosign_$unsafe_name.out" >&2
        exit 1
    fi
done

for safe_version in 'GitVersion: v3.1.3' 'GitVersion: v3.99.0'; do
    safe_name="${safe_version##*v}"
    safe_name="${safe_name//./_}"
    CHECKSUM_OVERRIDE="$archive_sha256" WITNESS_MODE=valid \
        COSIGN_VERSION_OUTPUT="$safe_version" \
        run_verification_case "verify_safe_cosign_$safe_name" bash
done

# The shim exits 43 if any custom trust setting reaches it. Success therefore
# proves the installer clears all three overrides for the verifier subprocess.
CHECKSUM_OVERRIDE="$archive_sha256" WITNESS_MODE=valid \
    SIGSTORE_ROOT_FILE="$tmp/attacker-root.json" \
    SIGSTORE_REKOR_PUBLIC_KEY="$tmp/attacker-rekor.pub" \
    SIGSTORE_CT_LOG_PUBLIC_KEY_FILE="$tmp/attacker-ctfe.pub" \
    run_verification_case verify_trust_isolation bash

for failure_mode in malformed_bundle invalid_signature; do
    witness_mode=valid
    [ "$failure_mode" = "malformed_bundle" ] && witness_mode=malformed_bundle
    if CHECKSUM_OVERRIDE="$archive_sha256" WITNESS_MODE="$witness_mode" COSIGN_VERIFY_RC=41 \
        run_verification_case "verify_$failure_mode" bash; then
        echo "FAIL: $failure_mode must abort archive verification" >&2
        exit 1
    fi
    if ! grep -q 'Sigstore verification failed' "$tmp/verify_$failure_mode.out"; then
        echo "FAIL: $failure_mode did not surface the Sigstore verification failure" >&2
        exit 1
    fi
done

: >"$tmp/verify_success.cosign.log"
CHECKSUM_OVERRIDE='' WITNESS_MODE=valid COSIGN_VERIFY_RC=0 run_verification_case verify_success bash
expected_cosign_log="$tmp/verify_success.cosign.expected"
printf '%s\n' \
    'verify-blob' \
    '--new-bundle-format' \
    '--bundle' \
    "$tmp/verify_success/release.sigstore.json" \
    '--certificate-identity' \
    'https://github.com/Dicklesworthstone/mcp_agent_mail_rust/.github/workflows/dist.yml@refs/tags/v9.9.9' \
    '--certificate-oidc-issuer' \
    'https://token.actions.githubusercontent.com' \
    "$archive_file" >"$expected_cosign_log"
if ! cmp -s "$expected_cosign_log" "$tmp/verify_success.cosign.log"; then
    echo "FAIL: cosign was not invoked with the exact release identity/issuer contract" >&2
    diff -u "$expected_cosign_log" "$tmp/verify_success.cosign.log" >&2 || true
    exit 1
fi

step "scenario H: release tags normalize narrowly and another tag cannot authenticate"
contract_output=$(EXTRACT="$extract" REQUESTED_VERSION='9.9.9-rc.1' bash -c '
    set -euo pipefail
    source "$EXTRACT"
    VERSION="$REQUESTED_VERSION"
    establish_release_contract
    printf "%s\n%s\n%s\n" "$VERSION" "$EXPECTED_RELEASE_VERSION" "$COSIGN_IDENTITY"
')
expected_contract_output=$(printf '%s\n' \
    'v9.9.9-rc.1' \
    '9.9.9-rc.1' \
    'https://github.com/Dicklesworthstone/mcp_agent_mail_rust/.github/workflows/dist.yml@refs/tags/v9.9.9-rc.1')
if [ "$contract_output" != "$expected_contract_output" ]; then
    echo "FAIL: Unix release contract did not canonicalize the exact requested tag" >&2
    printf 'Expected:\n%s\nObserved:\n%s\n' "$expected_contract_output" "$contract_output" >&2
    exit 1
fi
for invalid_version in 'v9.9.9+build' 'release-v9.9.9' 'v9.9.9/../../other'; do
    if EXTRACT="$extract" REQUESTED_VERSION="$invalid_version" bash -c '
        set -euo pipefail
        source "$EXTRACT"
        VERSION="$REQUESTED_VERSION"
        establish_release_contract
    '; then
        echo "FAIL: invalid release version was accepted: $invalid_version" >&2
        exit 1
    fi
done

if CHECKSUM_OVERRIDE="$archive_sha256" WITNESS_MODE=valid COSIGN_VERIFY_RC=0 \
    COSIGN_CERTIFICATE_IDENTITY='https://github.com/Dicklesworthstone/mcp_agent_mail_rust/.github/workflows/dist.yml@refs/tags/v9.9.8' \
    run_verification_case verify_wrong_tag bash; then
    echo "FAIL: a valid older-tag certificate must not authenticate v9.9.9" >&2
    exit 1
fi
if ! grep -q 'Sigstore verification failed' "$tmp/verify_wrong_tag.out"; then
    echo "FAIL: wrong-tag replay did not surface an actionable Sigstore failure" >&2
    cat "$tmp/verify_wrong_tag.out" >&2
    exit 1
fi

no_verify_line=$(grep -nF 'if [ "$NO_VERIFY" -eq 1 ]; then' "$INSTALL_SH" | tail -1 | cut -d: -f1)
verify_call_line=$(grep -nF 'verify_release_archive "$TMP/$TAR" "$URL" "$TAR"' "$INSTALL_SH" | cut -d: -f1)
members_line=$(grep -nF 'verify_archive_members_exact "$TMP/$TAR"' "$INSTALL_SH" | cut -d: -f1)
extract_line=$(grep -nF 'tar -xf "$TMP/$TAR" -C "$EXTRACT_DIR"' "$INSTALL_SH" | cut -d: -f1)
verify_gate_end_line=$(awk -v first="$verify_call_line" -v last="$members_line" \
    'NR > first && NR < last && $0 == "fi" { line = NR } END { print line }' "$INSTALL_SH")
verify_gate_fi_count=$(awk -v first="$verify_call_line" -v last="$members_line" \
    'NR > first && NR < last && $0 == "fi" { count++ } END { print count + 0 }' "$INSTALL_SH")
if ! grep -Fxq 'NO_VERIFY=0' "$INSTALL_SH" || \
    ! grep -Fq -- '--no-verify) NO_VERIFY=1; shift;;' "$INSTALL_SH"; then
    echo "FAIL: Unix archive verification must default on and require an explicit --no-verify escape" >&2
    exit 1
fi
if [ -z "$no_verify_line" ] || [ -z "$verify_call_line" ] || [ -z "$verify_gate_end_line" ] || \
    [ "$verify_gate_fi_count" -ne 2 ] || [ -z "$members_line" ] || [ -z "$extract_line" ] || \
    [ "$no_verify_line" -ge "$verify_call_line" ] || \
    [ "$verify_call_line" -ge "$verify_gate_end_line" ] || [ "$verify_gate_end_line" -ge "$members_line" ] || \
    [ "$members_line" -ge "$extract_line" ]; then
    echo "FAIL: Unix cryptographic and member gates must be explicit and precede extraction" >&2
    exit 1
fi

step "scenario I: archive inventory and staged versions are exact before replacement"
member_root="$tmp/archive-member-fixtures"
mkdir -p "$member_root/exact" "$member_root/extra" "$member_root/nested/bundle" \
    "$member_root/missing" "$member_root/symlink"

make_version_fixture() {
    local path="$1"
    local output="$2"
    printf '#!/usr/bin/env bash\nprintf "%%s\\n" %q\n' "$output" >"$path"
    chmod +x "$path"
}

make_version_fixture "$member_root/exact/am" 'am 9.9.9'
make_version_fixture "$member_root/exact/mcp-agent-mail" 'mcp-agent-mail 9.9.9'
cp "$member_root/exact/am" "$member_root/extra/am"
cp "$member_root/exact/mcp-agent-mail" "$member_root/extra/mcp-agent-mail"
printf '%s\n' unexpected >"$member_root/extra/README.txt"
cp "$member_root/exact/am" "$member_root/nested/bundle/am"
cp "$member_root/exact/mcp-agent-mail" "$member_root/nested/bundle/mcp-agent-mail"
cp "$member_root/exact/am" "$member_root/missing/am"
cp "$member_root/exact/am" "$member_root/symlink/am"
ln -s "$member_root/exact/mcp-agent-mail" "$member_root/symlink/mcp-agent-mail"

tar -cf "$member_root/exact.tar" -C "$member_root/exact" am mcp-agent-mail
tar -cf "$member_root/extra.tar" -C "$member_root/extra" am mcp-agent-mail README.txt
tar -cf "$member_root/nested.tar" -C "$member_root/nested" bundle/am bundle/mcp-agent-mail
tar -cf "$member_root/missing.tar" -C "$member_root/missing" am
tar -cf "$member_root/symlink.tar" -C "$member_root/symlink" am mcp-agent-mail

run_member_case() {
    local archive="$1"
    EXTRACT="$extract" CASE_TMP="$member_root" ARCHIVE_FILE="$archive" bash -c '
        set -euo pipefail
        source "$EXTRACT"
        TMP="$CASE_TMP"
        BIN_CLI=am
        BIN_SERVER=mcp-agent-mail
        verify_archive_members_exact "$ARCHIVE_FILE"
    '
}

run_member_case "$member_root/exact.tar"
for bad_archive in extra nested missing symlink; do
    if run_member_case "$member_root/$bad_archive.tar"; then
        echo "FAIL: $bad_archive archive inventory must be rejected" >&2
        exit 1
    fi
done

version_root="$tmp/version-fixtures"
mkdir -p "$version_root/good" "$version_root/wrong-cli" "$version_root/wrong-server" \
    "$version_root/extra-lines"
make_version_fixture "$version_root/good/am" 'am 9.9.9'
make_version_fixture "$version_root/good/mcp-agent-mail" 'mcp-agent-mail 9.9.9'
make_version_fixture "$version_root/wrong-cli/am" 'am 9.9.8'
make_version_fixture "$version_root/wrong-cli/mcp-agent-mail" 'mcp-agent-mail 9.9.9'
make_version_fixture "$version_root/wrong-server/am" 'am 9.9.9'
make_version_fixture "$version_root/wrong-server/mcp-agent-mail" 'mcp-agent-mail 9.9.8'
printf '#!/usr/bin/env bash\nprintf "am 9.9.9\\n\\n"\n' >"$version_root/extra-lines/am"
chmod +x "$version_root/extra-lines/am"
make_version_fixture "$version_root/extra-lines/mcp-agent-mail" 'mcp-agent-mail 9.9.9'

run_version_case() {
    local fixture_dir="$1"
    EXTRACT="$extract" CASE_TMP="$version_root" FIXTURE_DIR="$fixture_dir" bash -c '
        set -euo pipefail
        source "$EXTRACT"
        TMP="$CASE_TMP"
        BIN_CLI=am
        BIN_SERVER=mcp-agent-mail
        VERSION=v9.9.9
        EXPECTED_RELEASE_VERSION=9.9.9
        verify_release_binaries_exact "$FIXTURE_DIR/mcp-agent-mail" "$FIXTURE_DIR/am" "Staged"
    '
}

run_version_case "$version_root/good"
for bad_versions in wrong-cli wrong-server extra-lines; do
    if run_version_case "$version_root/$bad_versions"; then
        echo "FAIL: $bad_versions fixture must be rejected before replacement" >&2
        exit 1
    fi
done

step "scenario J: Unix pair replacement is digest-bound and rolls back as one unit"
transaction_root="$tmp/transaction-fixtures"
mkdir -p "$transaction_root/old" "$transaction_root/new" \
    "$transaction_root/wrong" "$transaction_root/install"
make_version_fixture "$transaction_root/old/am" 'am 9.9.8'
make_version_fixture "$transaction_root/old/mcp-agent-mail" 'mcp-agent-mail 9.9.8'
make_version_fixture "$transaction_root/new/am" 'am 9.9.9'
make_version_fixture "$transaction_root/new/mcp-agent-mail" 'mcp-agent-mail 9.9.9'
make_version_fixture "$transaction_root/wrong/am" 'am 9.9.9'
make_version_fixture "$transaction_root/wrong/mcp-agent-mail" 'mcp-agent-mail 9.9.8'
cp "$transaction_root/old/am" "$transaction_root/install/am"
cp "$transaction_root/old/mcp-agent-mail" "$transaction_root/install/mcp-agent-mail"

run_transaction_case() {
    local source_dir="$1"
    local fault_after_first_replace="${2:-0}"
    EXTRACT="$extract" TX_ROOT="$transaction_root" SOURCE_DIR="$source_dir" \
        FAULT_AFTER_FIRST_REPLACE="$fault_after_first_replace" bash -c '
            set -euo pipefail
            source "$EXTRACT"
            TMP="$TX_ROOT"
            BIN_CLI=am
            BIN_SERVER=mcp-agent-mail
            VERSION=v9.9.9
            EXPECTED_RELEASE_VERSION=9.9.9
            install_binary_pair_transactional \
                "$SOURCE_DIR/mcp-agent-mail" "$SOURCE_DIR/am" \
                "$TX_ROOT/install" "$FAULT_AFTER_FIRST_REPLACE"
        '
}

if run_transaction_case "$transaction_root/new" 1; then
    echo "FAIL: injected failure after the first replacement must abort the pair transaction" >&2
    exit 1
fi
if ! cmp -s "$transaction_root/old/am" "$transaction_root/install/am" || \
    ! cmp -s "$transaction_root/old/mcp-agent-mail" "$transaction_root/install/mcp-agent-mail"; then
    echo "FAIL: first-replacement failure did not restore the old binary pair byte-for-byte" >&2
    exit 1
fi

run_transaction_case "$transaction_root/new"
if ! cmp -s "$transaction_root/new/am" "$transaction_root/install/am" || \
    ! cmp -s "$transaction_root/new/mcp-agent-mail" "$transaction_root/install/mcp-agent-mail"; then
    echo "FAIL: successful pair transaction did not preserve staged bytes exactly" >&2
    exit 1
fi

if run_transaction_case "$transaction_root/wrong"; then
    echo "FAIL: installed version mismatch must abort the pair transaction" >&2
    exit 1
fi
if ! cmp -s "$transaction_root/new/am" "$transaction_root/install/am" || \
    ! cmp -s "$transaction_root/new/mcp-agent-mail" "$transaction_root/install/mcp-agent-mail"; then
    echo "FAIL: post-install version failure did not restore the prior binary pair" >&2
    exit 1
fi
if find "$transaction_root/install" -maxdepth 1 -name '*.bak.preinstall-*' -print -quit | grep -q .; then
    echo "FAIL: completed or successfully rolled-back transaction retained an unexpected backup" >&2
    exit 1
fi

staged_version_line=$(grep -nF 'verify_release_binaries_exact "$SERVER_BIN" "$CLI_BIN" "Staged"' "$INSTALL_SH" | cut -d: -f1)
replace_line=$(grep -nF 'install_binary_pair_transactional "$SERVER_BIN" "$CLI_BIN" "$DEST"' "$INSTALL_SH" | cut -d: -f1)
transaction_start_line=$(grep -nF 'install_binary_pair_transactional() {' "$INSTALL_SH" | cut -d: -f1)
transaction_end_line=$(awk -v first="$transaction_start_line" 'NR > first && $0 == "}" { print NR; exit }' "$INSTALL_SH")
installed_version_line=$(grep -nF '! verify_release_binaries_exact "$server_dest" "$cli_dest" "Installed"' "$INSTALL_SH" | cut -d: -f1)
if [ -z "$staged_version_line" ] || [ -z "$replace_line" ] || \
    [ -z "$transaction_start_line" ] || [ -z "$transaction_end_line" ] || \
    [ -z "$installed_version_line" ] || [ "$staged_version_line" -ge "$replace_line" ] || \
    [ "$installed_version_line" -le "$transaction_start_line" ] || \
    [ "$installed_version_line" -ge "$transaction_end_line" ]; then
    echo "FAIL: Unix exact staged/installed version checks do not enclose the pair transaction" >&2
    exit 1
fi
if grep -Fq 'atomic_install "$SERVER_BIN"' "$INSTALL_SH"; then
    echo "FAIL: Unix main flow still performs independent per-binary replacement" >&2
    exit 1
fi

for source_control in \
    'fetch --quiet --depth 1 origin "refs/tags/${release_tag}"' \
    'release_dependency_pin "$TMP/src" FRANKENSEARCH_COMMIT' \
    'release_dependency_pin "$TMP/src" FAST_CMAES_COMMIT' \
    'release_dependency_pin "$TMP/src" BEADS_RUST_COMMIT' \
    'CARGO_TARGET_DIR="$source_target_dir"' \
    'cargo build --locked --release -p mcp-agent-mail -p mcp-agent-mail-cli' \
    'The installer will not silently substitute a source build.'; do
    if ! grep -Fq "$source_control" "$INSTALL_SH"; then
        echo "FAIL: exact-tag source-build control missing: $source_control" >&2
        exit 1
    fi
done
if grep -Fq 'binary-download:fallback_to_source' "$INSTALL_SH"; then
    echo "FAIL: release archive failure still silently changes into a source build" >&2
    exit 1
fi
if [ "$(grep -cF 'FROM_SOURCE=1' "$INSTALL_SH")" -ne 1 ] || \
    ! grep -Fq -- '--from-source) FROM_SOURCE=1; shift;;' "$INSTALL_SH"; then
    echo "FAIL: source execution can be enabled without the explicit --from-source option" >&2
    exit 1
fi
if grep -Fq 'resolve_version:fallback_default' "$INSTALL_SH" || \
    grep -Fq 'defaulting to $VERSION' "$INSTALL_SH"; then
    echo "FAIL: latest-release resolution still fails open to a hard-coded version" >&2
    exit 1
fi

step "scenario K: PowerShell installer statically enforces the same exact-release contract"
if ! grep -Fq 'COSIGN_IDENTITY="https://github.com/Dicklesworthstone/mcp_agent_mail_rust/.github/workflows/dist.yml@refs/tags/${VERSION}"' "$INSTALL_SH" || \
    ! grep -Fq 'CertificateIdentity = "https://github.com/Dicklesworthstone/mcp_agent_mail_rust/.github/workflows/dist.yml@refs/tags/$normalizedTag"' "$INSTALL_PS1"; then
    echo "FAIL: installers do not derive the certificate identity from the exact normalized tag" >&2
    exit 1
fi
if grep -Fq 'refs/tags/.+$' "$INSTALL_SH" || grep -Fq 'refs/tags/.+$' "$INSTALL_PS1" || \
    grep -Fq -- '--certificate-identity-regexp' "$INSTALL_SH" || \
    grep -Fq -- '--certificate-identity-regexp' "$INSTALL_PS1"; then
    echo "FAIL: broad cross-tag certificate identity matching remains in an installer" >&2
    exit 1
fi
if ! grep -Fq "COSIGN_OIDC_ISSUER='https://token.actions.githubusercontent.com'" "$INSTALL_SH" || \
    ! grep -Fq "\$CosignOidcIssuer = 'https://token.actions.githubusercontent.com'" "$INSTALL_PS1"; then
    echo "FAIL: installers do not share the exact GitHub Actions OIDC issuer" >&2
    exit 1
fi

ps_gate_line=$(grep -nF 'if ($ShouldVerifyArchive) {' "$INSTALL_PS1" | cut -d: -f1)
ps_sigstore_line=$(grep -nF 'Verify-SigstoreBundle -FilePath $zipPath' "$INSTALL_PS1" | cut -d: -f1)
ps_members_line=$(grep -nF 'Assert-ExactArchiveMembers -ArchivePath $zipPath' "$INSTALL_PS1" | cut -d: -f1)
ps_extract_line=$(grep -nF 'Expand-Archive -LiteralPath $zipPath' "$INSTALL_PS1" | cut -d: -f1)
ps_gate_end_line=$(awk -v first="$ps_sigstore_line" -v last="$ps_members_line" \
    'NR > first && NR < last && $0 == "    }" { line = NR } END { print line }' "$INSTALL_PS1")
ps_staged_version_line=$(grep -nF 'Assert-ExactBinaryVersion -BinaryPath $amSource -ExpectedOutput "am $requestedNormalized" -Phase "Staged"' "$INSTALL_PS1" | cut -d: -f1)
ps_replace_line=$(grep -nF '    Install-BinariesAtomically `' "$INSTALL_PS1" | cut -d: -f1)
ps_post_version_line=$(grep -nF 'Verify-Install -InstallDir $VerifiedInstallDir -ExpectedVersion $requestedNormalized' "$INSTALL_PS1" | cut -d: -f1)
ps_callback_line=$(grep -nF '& $PostInstallVerifier $InstallDir' "$INSTALL_PS1" | cut -d: -f1)
ps_commit_line=$(grep -nF '        $committed = $true' "$INSTALL_PS1" | cut -d: -f1)
ps_backup_cleanup_line=$(grep -nF '        if ($committed) {' "$INSTALL_PS1" | cut -d: -f1)
if ! grep -Fq '$ShouldVerifyArchive = if ($NoVerify) { $false } else { $true }' "$INSTALL_PS1"; then
    echo "FAIL: PowerShell archive verification must default on and require an explicit -NoVerify escape" >&2
    exit 1
fi
if [ -z "$ps_gate_line" ] || [ -z "$ps_sigstore_line" ] || [ -z "$ps_gate_end_line" ] || \
    [ -z "$ps_members_line" ] || [ -z "$ps_extract_line" ] || \
    [ "$ps_gate_line" -ge "$ps_sigstore_line" ] || [ "$ps_sigstore_line" -ge "$ps_gate_end_line" ] || \
    [ "$ps_gate_end_line" -ge "$ps_members_line" ] || [ "$ps_members_line" -ge "$ps_extract_line" ]; then
    echo "FAIL: PowerShell cryptographic and member gates must precede Expand-Archive" >&2
    exit 1
fi
if [ -z "$ps_staged_version_line" ] || [ -z "$ps_replace_line" ] || \
    [ -z "$ps_post_version_line" ] || [ -z "$ps_callback_line" ] || \
    [ -z "$ps_commit_line" ] || [ -z "$ps_backup_cleanup_line" ] || \
    [ "$ps_staged_version_line" -ge "$ps_post_version_line" ] || \
    [ "$ps_post_version_line" -ge "$ps_replace_line" ] || \
    [ "$ps_callback_line" -ge "$ps_commit_line" ] || \
    [ "$ps_commit_line" -ge "$ps_backup_cleanup_line" ]; then
    echo "FAIL: PowerShell post-install verification must commit before backup cleanup" >&2
    exit 1
fi
for required_text in \
    'Get-Command Get-FileHash -ErrorAction SilentlyContinue' \
    'Get-Command cosign -CommandType Application -ErrorAction SilentlyContinue' \
    'require >=v3.1.3 and <v4.0.0' \
    '"--new-bundle-format"' \
    '"SIGSTORE_ROOT_FILE"' \
    '"SIGSTORE_REKOR_PUBLIC_KEY"' \
    '"SIGSTORE_CT_LOG_PUBLIC_KEY_FILE"' \
    'ConvertFrom-Json -ErrorAction Stop' \
    '"--certificate-identity", $CosignIdentity' \
    "'^v?(?<version>[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z][0-9A-Za-z.-]*)?)$'" \
    '$CosignIdentity = $releaseContract.CertificateIdentity' \
    '$entries.Count -eq 2' \
    '$versionLines.Count -ne 1' \
    'Assert-ExactBinaryVersion -BinaryPath $serverSource -ExpectedOutput "mcp-agent-mail $requestedNormalized" -Phase "Staged"' \
    'Assert-ExactBinaryVersion -BinaryPath $serverExe -ExpectedOutput "mcp-agent-mail $ExpectedVersion" -Phase "Post-install"' \
    '$installerMutex = Enter-InstallerMutex -InstallDir $Dest' \
    '"Global\mcp-agent-mail-install-$digest"' \
    '-PostInstallVerifier $postInstallVerifier' \
    'Installed binary bytes differ from the verified staged pair.' \
    'Archive-member and exact-version checks remain mandatory.' \
    'UNSAFE: archive checksum and Sigstore verification skipped (-NoVerify)' \
    'malicious bytes can run arbitrary code.'; do
    if ! grep -Fq "$required_text" "$INSTALL_PS1"; then
        echo "FAIL: PowerShell fail-closed control missing: $required_text" >&2
        exit 1
    fi
done

step "ALL SCENARIOS PASSED"
