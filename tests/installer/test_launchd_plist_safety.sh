#!/usr/bin/env bash
#
# Verifies that install.sh's LaunchAgent plist writer refuses symlinked service
# paths before writing. The full installer is intentionally not sourced because
# it performs network and installation work at the bottom of the script.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
INSTALL_SH="$REPO_ROOT/install.sh"

if [ ! -f "$INSTALL_SH" ]; then
    echo "FATAL: $INSTALL_SH not found" >&2
    exit 2
fi

ts() { date -u +%Y-%m-%dT%H:%M:%SZ; }
step() { echo "[LAUNCHD_PLIST_SAFETY_TEST $(ts)] $*" >&2; }
fail() {
    echo "FAIL: $*" >&2
    exit 1
}

tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-install-launchd-safety.XXXXXX")"
trap 'step "temp left for inspection: $tmp"' EXIT

extract="$tmp/launchd_plist_functions.sh"
extract_function() {
    local fn="$1"
    awk -v fn="$fn" '
        $0 == fn "() {" {
            in_fn = 1
            print
            next
        }
        in_fn {
            print
            if ($0 == "}") {
                exit
            }
        }
    ' "$INSTALL_SH"
}

{
    printf '%s\n' 'warn() { printf "%s\n" "$*" >&2; }'
    printf '%s\n' 'verbose() { printf "%s\n" "$*" >&2; }'
    printf '%s\n' 'info() { :; }'
    printf '%s\n' 'ok() { :; }'
    printf '%s\n' 'err() { :; }'
    extract_function trim_ascii_whitespace
    extract_function strip_wrapping_quotes
    extract_function parse_env_assignment_rhs
    extract_function read_env_assignment_value
    extract_function rust_config_env_path
    extract_function generate_bearer_token
    extract_function desired_service_bind_host
    extract_function desired_service_bind_port
    extract_function plist_xml_escape
    extract_function plist_string_entry
    extract_function plist_env_entry
    extract_function ensure_real_directory_tree
    extract_function ensure_real_file_target_path
    extract_function write_launchd_service_plist
    extract_function repair_launchd_service_env_from_rust_config
    extract_function has_remote_http_client_targets
    extract_function ensure_remote_http_client_readiness
} >"$extract"

for required in rust_config_env_path generate_bearer_token plist_xml_escape ensure_real_directory_tree ensure_real_file_target_path write_launchd_service_plist repair_launchd_service_env_from_rust_config has_remote_http_client_targets ensure_remote_http_client_readiness; do
    if ! grep -q "^${required}()" "$extract"; then
        fail "could not extract ${required} from install.sh"
    fi
done

# shellcheck source=/dev/null
source "$extract"

# The isolated repair scenarios intentionally exercise the writer even though
# their temporary DEST is not the installer's default service destination.
service_management_allowed() { return 0; }

step "scenario 0A: config.env authority honors only an absolute XDG override"
path_home="$tmp/path-home"
path_xdg="$tmp/path-xdg"
[ "$(HOME="$path_home" XDG_CONFIG_HOME="$path_xdg" rust_config_env_path)" = \
    "$path_xdg/mcp-agent-mail/config.env" ] \
    || fail "absolute XDG_CONFIG_HOME was not selected"
[ "$(HOME="$path_home" XDG_CONFIG_HOME='' rust_config_env_path)" = \
    "$path_home/.config/mcp-agent-mail/config.env" ] \
    || fail "empty XDG_CONFIG_HOME did not fall back to HOME"
[ "$(HOME="$path_home" XDG_CONFIG_HOME=relative-config rust_config_env_path)" = \
    "$path_home/.config/mcp-agent-mail/config.env" ] \
    || fail "relative XDG_CONFIG_HOME was not ignored"
if HOME=relative-home XDG_CONFIG_HOME=relative-config rust_config_env_path >/dev/null; then
    fail "relative HOME and XDG_CONFIG_HOME produced a credential path"
fi

step "scenario 0B: bearer generation rejects missing or malformed RNG output"
empty_path="$tmp/empty-path"
invalid_rng_path="$tmp/invalid-rng-path"
mkdir -p "$empty_path" "$invalid_rng_path"
cat >"$invalid_rng_path/openssl" <<'EOF'
#!/bin/sh
printf '%s' 'not-hex'
EOF
chmod 755 "$invalid_rng_path/openssl"
if PATH="$empty_path" generate_bearer_token >/dev/null; then
    fail "bearer generation succeeded without RNG tooling"
fi
if PATH="$invalid_rng_path" generate_bearer_token >/dev/null; then
    fail "bearer generation accepted malformed OpenSSL output"
fi

step "scenario 0C: cryptographic bearer sources produce valid distinct tokens"
if command -v openssl >/dev/null 2>&1; then
    openssl_token_a="$(generate_bearer_token)" || fail "OpenSSL bearer generation failed"
    openssl_token_b="$(generate_bearer_token)" || fail "second OpenSSL bearer generation failed"
    [ "${#openssl_token_a}" -eq 64 ] || fail "OpenSSL bearer has the wrong length"
    [ "$openssl_token_a" != "$openssl_token_b" ] || fail "OpenSSL bearers were not distinct"
fi
urandom_path="$tmp/urandom-path"
mkdir -p "$urandom_path"
for helper in head od tr; do
    helper_path="$(command -v "$helper")"
    [ -n "$helper_path" ] || fail "required urandom helper is unavailable: $helper"
    ln -s "$helper_path" "$urandom_path/$helper"
done
urandom_token_a="$(PATH="$urandom_path" generate_bearer_token)" \
    || fail "/dev/urandom bearer generation failed"
urandom_token_b="$(PATH="$urandom_path" generate_bearer_token)" \
    || fail "second /dev/urandom bearer generation failed"
[ "${#urandom_token_a}" -eq 64 ] || fail "/dev/urandom bearer has the wrong length"
[ "$urandom_token_a" != "$urandom_token_b" ] || fail "/dev/urandom bearers were not distinct"

write_test_plist() {
    local plist_path="$1"
    local home="$2"
    local storage_root="$3"
    write_launchd_service_plist \
        "$plist_path" \
        "/opt/agent mail/bin/am" \
        "$home" \
        "$storage_root" \
        "sqlite:///$storage_root/storage.sqlite3" \
        "tok&en" \
        "127.0.0.1" \
        "8765" \
        "/mcp/?x=<y>"
}

step "scenario A: normal plist write succeeds and XML-escapes values"
home_a="$tmp/home-a"
storage_a="$tmp/storage-a"
plist_a="$home_a/Library/LaunchAgents/com.agent-mail.plist"
write_test_plist "$plist_a" "$home_a" "$storage_a" || fail "normal plist write failed"
grep -q '<string>/opt/agent mail/bin/am</string>' "$plist_a" || fail "missing am binary argument"
grep -q '<string>tok&amp;en</string>' "$plist_a" || fail "token was not XML-escaped"
grep -q '<string>/mcp/?x=&lt;y&gt;</string>' "$plist_a" || fail "HTTP_PATH was not XML-escaped"
if stat -f '%Lp' "$plist_a" >/dev/null 2>&1; then
    plist_a_mode="$(stat -f '%Lp' "$plist_a")"
else
    plist_a_mode="$(stat -c '%a' "$plist_a")"
fi
[ "$plist_a_mode" = "600" ] || fail "credential-bearing plist mode was $plist_a_mode, expected 600"

step "scenario B: symlinked plist target is rejected without mutating target"
home_b="$tmp/home-b"
storage_b="$tmp/storage-b"
mkdir -p "$home_b/Library/LaunchAgents"
outside_plist="$tmp/outside.plist"
plist_b="$home_b/Library/LaunchAgents/com.agent-mail.plist"
printf 'do not overwrite\n' >"$outside_plist"
ln -s "$outside_plist" "$plist_b"
if write_test_plist "$plist_b" "$home_b" "$storage_b"; then
    fail "symlinked plist target unexpectedly succeeded"
fi
[ "$(cat "$outside_plist")" = "do not overwrite" ] || fail "symlinked plist target was overwritten"

step "scenario C: symlinked LaunchAgents directory is rejected"
home_c="$tmp/home-c"
outside_agents="$tmp/outside-agents"
storage_c="$tmp/storage-c"
mkdir -p "$home_c/Library" "$outside_agents"
ln -s "$outside_agents" "$home_c/Library/LaunchAgents"
plist_c="$home_c/Library/LaunchAgents/com.agent-mail.plist"
if write_test_plist "$plist_c" "$home_c" "$storage_c"; then
    fail "symlinked LaunchAgents directory unexpectedly succeeded"
fi
[ ! -e "$outside_agents/com.agent-mail.plist" ] || fail "plist was written through symlinked LaunchAgents directory"

step "scenario D: symlinked storage root is rejected before plist is written"
home_d="$tmp/home-d"
outside_storage="$tmp/outside-storage"
storage_d="$tmp/storage-link"
plist_d="$home_d/Library/LaunchAgents/com.agent-mail.plist"
mkdir -p "$outside_storage"
ln -s "$outside_storage" "$storage_d"
if write_test_plist "$plist_d" "$home_d" "$storage_d"; then
    fail "symlinked storage root unexpectedly succeeded"
fi
[ ! -e "$plist_d" ] || fail "plist was written despite symlinked storage root"

step "scenario E: literal parent traversal path is rejected"
if ensure_real_directory_tree ".." "LaunchAgent directory"; then
    fail "literal parent traversal path unexpectedly succeeded"
fi

step "scenario F: glob metacharacters are treated literally"
glob_dir="$tmp/glob-[literal]-*"
ensure_real_directory_tree "$glob_dir" "LaunchAgent directory" || fail "literal glob path was rejected"
[ -d "$glob_dir" ] || fail "literal glob path was not created"

step "scenario G: LaunchAgent env repair does not depend on python plistlib"
home_g="$tmp/home-g"
storage_g="$tmp/storage from config"
plist_g="$home_g/Library/LaunchAgents/com.agent-mail.plist"
fake_bin="$tmp/fake-bin"
python_marker="$tmp/python-was-invoked"
launchctl_log="$tmp/launchctl.log"
mkdir -p "$home_g/Library/LaunchAgents" "$home_g/.config/mcp-agent-mail" "$fake_bin"
printf 'stale plist\n' >"$plist_g"
cat >"$home_g/.config/mcp-agent-mail/config.env" <<EOF
STORAGE_ROOT="$storage_g"
DATABASE_URL="sqlite:///$storage_g/storage.sqlite3"
HTTP_BEARER_TOKEN="repair&token"
HTTP_HOST=127.0.0.1
HTTP_PORT=9876
HTTP_PATH="/mcp/repair"
EOF
cat >"$fake_bin/python3" <<EOF
#!/usr/bin/env bash
printf 'python3 should not be invoked\n' >"$python_marker"
exit 70
EOF
cat >"$fake_bin/launchctl" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$launchctl_log"
exit 0
EOF
chmod 755 "$fake_bin/python3" "$fake_bin/launchctl"
(
    export OS=darwin
    export HOME="$home_g"
    export DEST="/opt/agent mail/bin"
    export BIN_CLI="am"
    export PATH="$fake_bin:$PATH"
    unset RUST_STORAGE_ROOT
    repair_launchd_service_env_from_rust_config
)
[ ! -e "$python_marker" ] || fail "repair path invoked python3"
grep -Fq '<string>/opt/agent mail/bin/am</string>' "$plist_g" || fail "repair path did not write am ProgramArguments"
grep -Fq "<string>sqlite:///$storage_g/storage.sqlite3</string>" "$plist_g" || fail "repair path did not use DATABASE_URL from config.env"
grep -Fq '<string>repair&amp;token</string>' "$plist_g" || fail "repair path did not XML-escape HTTP_BEARER_TOKEN"
grep -Fq '<string>/mcp/repair</string>' "$plist_g" || fail "repair path did not use HTTP_PATH from config.env"
grep -q "^bootout " "$launchctl_log" || fail "repair path did not restart existing launchd service"
grep -q "^bootstrap " "$launchctl_log" || fail "repair path did not bootstrap launchd service"

step "scenario H: plist rewrite failure propagates to the caller"
if (
    write_launchd_service_plist() { return 1; }
    export OS=darwin
    export HOME="$home_g"
    export DEST="/opt/agent mail/bin"
    export BIN_CLI="am"
    export PATH="$fake_bin:$PATH"
    unset RUST_STORAGE_ROOT
    repair_launchd_service_env_from_rust_config
); then
    fail "plist rewrite failure was reported as successful"
fi

step "scenario I: launchctl bootstrap failure propagates to the caller"
cat >"$fake_bin/launchctl" <<EOF
#!/usr/bin/env bash
printf '%s\n' "\$*" >>"$launchctl_log"
if [ "\${1:-}" = "bootstrap" ]; then
    exit 69
fi
exit 0
EOF
chmod 755 "$fake_bin/launchctl"
if (
    export OS=darwin
    export HOME="$home_g"
    export DEST="/opt/agent mail/bin"
    export BIN_CLI="am"
    export PATH="$fake_bin:$PATH"
    unset RUST_STORAGE_ROOT
    repair_launchd_service_env_from_rust_config
); then
    fail "launchctl bootstrap failure was reported as successful"
fi

step "scenario J: OMP-only targets activate healthy and unhealthy readiness lanes"
omp_only_config="$tmp/omp-only-mcp.json"
printf '%s\n' '{}' >"$omp_only_config"
readiness_home="$tmp/readiness-home"
readiness_path="$tmp/readiness-empty-path"
readiness_dest="$tmp/readiness-bin"
mkdir -p "$readiness_home" "$readiness_path" "$readiness_dest"
remote_scan_mode=omp
detect_mcp_configs() {
    if [ "$remote_scan_mode" = "omp" ]; then
        printf 'omp\t%s\t1\n' "$omp_only_config"
    fi
}
desired_mcp_http_url() { printf '%s' 'http://127.0.0.1:8765/mcp/'; }
REMOTE_PROBE_CALLS=0
probe_remote_http_endpoint() {
    REMOTE_PROBE_CALLS=$((REMOTE_PROBE_CALLS + 1))
    return 0
}
HOME="$readiness_home" PATH="$readiness_path" ensure_remote_http_client_readiness \
    || fail "healthy OMP-only readiness lane failed"
[ "$REMOTE_PROBE_CALLS" -eq 1 ] || fail "OMP-only target did not run the healthy endpoint probe"

cat >"$readiness_dest/am" <<'EOF'
#!/bin/sh
exit 64
EOF
chmod 755 "$readiness_dest/am"
probe_remote_http_endpoint() {
    REMOTE_PROBE_CALLS=$((REMOTE_PROBE_CALLS + 1))
    return 1
}
service_management_allowed() { return 0; }
platform_supports_user_service_management() { return 0; }
DEST="$readiness_dest"
BIN_CLI=am
if HOME="$readiness_home" PATH="$readiness_path" ensure_remote_http_client_readiness; then
    fail "unhealthy OMP-only readiness lane was reported as successful"
fi

step "scenario K: readiness still skips when no remote HTTP client is present"
remote_scan_mode=none
REMOTE_PROBE_CALLS=0
if ! HOME="$readiness_home" PATH="$readiness_path" ensure_remote_http_client_readiness; then
    fail "no-client readiness skip returned failure"
fi
[ "$REMOTE_PROBE_CALLS" -eq 0 ] || fail "no-client readiness skip still probed the endpoint"

step "ALL SCENARIOS PASSED"
