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
    extract_function private_file_identity
    extract_function private_file_link_count
    extract_function private_file_security_identity
    extract_function ensure_private_file_target_path
    extract_function write_private_file_atomic
    extract_function write_launchd_service_plist
    extract_function repair_launchd_service_env_from_rust_config
    extract_function detect_mcp_configs
    extract_function remote_http_client_target_tools
    extract_function has_remote_http_client_targets
    extract_function ensure_remote_http_client_readiness
    extract_function configure_mcp_clients_for_install
    sed -n '/^install_legacy_launcher_takeover_shims() {/,/^# T1\.5:/p' "$INSTALL_SH" | sed '$d'
} >"$extract"

for required in rust_config_env_path generate_bearer_token plist_xml_escape ensure_real_directory_tree ensure_real_file_target_path private_file_identity private_file_link_count private_file_security_identity ensure_private_file_target_path write_private_file_atomic write_launchd_service_plist repair_launchd_service_env_from_rust_config detect_mcp_configs remote_http_client_target_tools has_remote_http_client_targets ensure_remote_http_client_readiness configure_mcp_clients_for_install install_legacy_launcher_takeover_shims; do
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

step "scenario I2: production target discovery never resolves a relative HOME from cwd"
relative_home_root="$tmp/relative-home-root"
relative_home_path="$tmp/relative-home-empty-path"
mkdir -p "$relative_home_root/relative-home/.omp/agent" "$relative_home_path"
printf '%s\n' '{}' >"$relative_home_root/relative-home/.omp/agent/mcp.json"
relative_home_targets="$(
    cd "$relative_home_root"
    unset APPDATA OMP_PROFILE PI_PROFILE PI_CONFIG_DIR PI_CODING_AGENT_DIR
    HOME=relative-home PATH="$relative_home_path" remote_http_client_target_tools
)"
[ -z "$relative_home_targets" ] \
    || fail "production detector accepted cwd-relative HOME authority"

relative_agent_root="$tmp/relative-agent-root"
relative_agent_home="$tmp/relative-agent-home"
mkdir -p "$relative_agent_root/relative-agent" "$relative_agent_home"
printf '%s\n' '{}' >"$relative_agent_root/relative-agent/mcp.json"
set +e
relative_agent_targets="$(
    cd "$relative_agent_root"
    unset APPDATA OMP_PROFILE PI_PROFILE PI_CONFIG_DIR
    HOME="$relative_agent_home" PI_CODING_AGENT_DIR=relative-agent \
        PATH="$relative_home_path" remote_http_client_target_tools
)"
relative_agent_rc=$?
set -e
[ "$relative_agent_rc" -eq 2 ] \
    || fail "cwd-relative PI_CODING_AGENT_DIR returned $relative_agent_rc instead of 2"
[ -z "$relative_agent_targets" ] \
    || fail "production detector emitted a cwd-relative OMP target"

symlink_config_home="$tmp/symlink-config-home"
symlink_config_target="$tmp/symlink-config-target"
mkdir -p "$symlink_config_home" "$symlink_config_target/nested/agent"
printf '%s\n' '{}' >"$symlink_config_target/nested/agent/mcp.json"
ln -s "$symlink_config_target" "$symlink_config_home/custom-root"
set +e
symlink_config_targets="$(
    unset APPDATA OMP_PROFILE PI_PROFILE PI_CODING_AGENT_DIR
    HOME="$symlink_config_home" PI_CONFIG_DIR=custom-root/nested \
        PATH="$relative_home_path" remote_http_client_target_tools
)"
symlink_config_rc=$?
set -e
[ "$symlink_config_rc" -eq 2 ] \
    || fail "symlinked PI_CONFIG_DIR ancestry returned $symlink_config_rc instead of 2"
[ -z "$symlink_config_targets" ] \
    || fail "production detector emitted an OMP target through symlinked PI_CONFIG_DIR ancestry"

set +e
traversal_config_targets="$(
    unset APPDATA OMP_PROFILE PI_PROFILE PI_CODING_AGENT_DIR
    HOME="$relative_agent_home" PI_CONFIG_DIR=../relative-agent-root/relative-agent \
        PATH="$relative_home_path" remote_http_client_target_tools
)"
traversal_config_rc=$?
set -e
[ "$traversal_config_rc" -eq 2 ] \
    || fail "parent-traversing PI_CONFIG_DIR returned $traversal_config_rc instead of 2"
[ -z "$traversal_config_targets" ] \
    || fail "production detector emitted an OMP target through PI_CONFIG_DIR traversal"

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
    REMOTE_HTTP_PROBE_DETAIL="stub healthy"
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
    REMOTE_HTTP_PROBE_DETAIL="stub unhealthy"
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

step "scenario K2: invalid OMP authority is a hard readiness failure"
detect_mcp_configs() { return 2; }
if HOME="$readiness_home" PATH="$readiness_path" ensure_remote_http_client_readiness; then
    fail "invalid OMP authority was treated as a no-client readiness skip"
else
    invalid_readiness_rc=$?
fi
[ "$invalid_readiness_rc" -eq 2 ] \
    || fail "invalid OMP authority returned $invalid_readiness_rc instead of 2"

step "scenario K3: production setup wrapper fails only for established client authority"
configure_mcp_clients() { return 1; }
remote_http_client_target_tools() { printf '%s\n' omp; }
if AM_INSTALL_SKIP_REMOTE_HTTP_READINESS=1 \
    configure_mcp_clients_for_install /unused/server /unused/am; then
    fail "readiness override suppressed a detected OMP setup failure"
fi
remote_http_client_target_tools() { printf '%s\n' codex; }
if configure_mcp_clients_for_install /unused/server /unused/am; then
    fail "detected Codex setup failure was suppressed"
fi
remote_http_client_target_tools() { return 0; }
configure_mcp_clients_for_install /unused/server /unused/am \
    || fail "genuine no-client setup failure became fatal"

configure_call_marker="$tmp/configure-call-marker"
configure_mcp_clients() { printf '%s\n' invoked >"$configure_call_marker"; return 0; }
remote_http_client_target_tools() { return 2; }
if configure_mcp_clients_for_install /unused/server /unused/am; then
    fail "invalid client authority was reported as successful"
else
    invalid_setup_rc=$?
fi
[ "$invalid_setup_rc" -eq 2 ] \
    || fail "invalid client authority returned $invalid_setup_rc instead of 2"
[ ! -e "$configure_call_marker" ] \
    || fail "setup writer ran after invalid client authority"

grep -Fq 'if ! configure_mcp_clients_for_install "$DEST/$BIN_SERVER" "$DEST/$BIN_CLI"; then' \
    "$INSTALL_SH" || fail "production installer does not call the failure-policy wrapper"
if grep -Eq 'configure_mcp_clients(_for_install)? .*\|\| true' "$INSTALL_SH"; then
    fail "production installer still suppresses MCP configuration failure"
fi

step "scenario K4: private atomic writer publishes one secure inode and refuses symlinks"
private_writer_dir="$tmp/private-writer"
private_writer_path="$private_writer_dir/config.env"
private_writer_peer="$private_writer_dir/config.env.peer"
private_writer_victim="$private_writer_dir/victim.env"
private_writer_symlink="$private_writer_dir/symlink.env"
mkdir -p "$private_writer_dir"
printf '%s\n' 'OLD=preserved' >"$private_writer_path"
chmod 600 "$private_writer_path"
ln "$private_writer_path" "$private_writer_peer"
printf '%s\n' 'NEW=private' \
    | write_private_file_atomic "$private_writer_path" "private writer control" \
    || fail "private writer ordinary hardlink-detaching replacement failed"
[ "$(cat "$private_writer_path")" = 'NEW=private' ] \
    || fail "private writer published the wrong content"
[ "$(cat "$private_writer_peer")" = 'OLD=preserved' ] \
    || fail "private writer modified the outside hardlink peer"
private_file_security_identity "$private_writer_path" >/dev/null \
    || fail "private writer did not publish a mode-600 single-link regular file"
printf '%s\n' 'VICTIM=unchanged' >"$private_writer_victim"
chmod 600 "$private_writer_victim"
ln -s "$private_writer_victim" "$private_writer_symlink"
if printf '%s\n' 'VICTIM=clobbered' \
    | write_private_file_atomic "$private_writer_symlink" "private writer symlink control"; then
    fail "private writer followed a symlink destination"
fi
[ "$(cat "$private_writer_victim")" = 'VICTIM=unchanged' ] \
    || fail "private writer changed a symlink target"
[ -L "$private_writer_symlink" ] \
    || fail "private writer replaced the planted symlink"

step "scenario K5: private atomic writer statically binds publication without path chmod"
private_security_body="$(extract_function private_file_security_identity)"
private_writer_body="$(extract_function write_private_file_atomic)"
printf '%s\n' "$private_security_body" | grep -Fq "stat -f '%d:%i:%HT:%Lp:%l'" \
    || fail "private writer lacks BSD no-follow type/mode/link identity"
printf '%s\n' "$private_security_body" | grep -Fq "stat -c '%d:%i:%F:%a:%h'" \
    || fail "private writer lacks GNU no-follow type/mode/link identity"
printf '%s\n' "$private_writer_body" | grep -Fq \
    "published_security_identity=\$(private_file_security_identity \"\$path\")" \
    || fail "private writer does not validate the published path identity"
printf '%s\n' "$private_writer_body" | grep -Fq \
    "[ \"\$published_security_identity\" != \"\$tmp_security_identity\" ]" \
    || fail "private writer does not bind publication to the validated tempfile"
if printf '%s\n' "$private_writer_body" | grep -Fq "chmod 600 \"\$path\""; then
    fail "private writer reopens a symlink-follow chmod race after publication"
fi

step "scenario L: generated legacy shim follows the absolute config.env authority contract"
legacy_clone="$tmp/legacy-clone"
legacy_rust_bin="$tmp/legacy-rust-bin"
legacy_home="$tmp/legacy-home"
legacy_xdg="$tmp/legacy-xdg"
mkdir -p "$legacy_rust_bin" \
    "$legacy_home/.config/mcp-agent-mail" \
    "$legacy_xdg/mcp-agent-mail"
cat >"$legacy_rust_bin/am" <<'EOF'
#!/bin/sh
printf '%s' "${HTTP_BEARER_TOKEN:-missing-token}"
EOF
chmod 755 "$legacy_rust_bin/am"
printf '%s\n' 'HTTP_BEARER_TOKEN=home-fallback-token' \
    >"$legacy_home/.config/mcp-agent-mail/config.env"
printf '%s\n' 'HTTP_BEARER_TOKEN=custom-xdg-token' \
    >"$legacy_xdg/mcp-agent-mail/config.env"
PYTHON_CLONE_FOUND=1
PYTHON_CLONE_PATH="$legacy_clone"
DEST="$legacy_rust_bin"
BIN_CLI=am
install_legacy_launcher_takeover_shims \
    || fail "could not generate isolated legacy takeover shim"
legacy_shim="$legacy_clone/scripts/run_server_with_token.sh"
[ -x "$legacy_shim" ] || fail "legacy takeover shim was not executable"
[ "$(HOME="$legacy_home" XDG_CONFIG_HOME="$legacy_xdg" "$legacy_shim")" = \
    "custom-xdg-token" ] || fail "legacy shim did not prefer custom XDG credential"
[ "$(HOME="$legacy_home" XDG_CONFIG_HOME=relative-config "$legacy_shim")" = \
    "home-fallback-token" ] || fail "legacy shim did not ignore relative XDG credential path"
[ "$(unset HOME; XDG_CONFIG_HOME="$legacy_xdg" "$legacy_shim")" = \
    "custom-xdg-token" ] || fail "legacy shim required HOME despite an absolute XDG authority"
if HOME=relative-home XDG_CONFIG_HOME=relative-config "$legacy_shim" >/dev/null 2>&1; then
    fail "legacy shim accepted relative HOME and XDG credential authorities"
fi

step "ALL SCENARIOS PASSED"
